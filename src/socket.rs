use axum::{
    Extension,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use sockudo_ws::{
    Message,
    axum_integration::{WebSocket, WebSocketUpgrade, WebSocketUpgradeRejection},
};

use crate::{
    AppState, status, token,
    types::{Data, Event, Value, valid_event_id},
};

mod pubsub;
mod sessions;
pub(crate) use pubsub::{PubSub, SubscriberId};
mod subscription;
pub(crate) use sessions::revoke_session;

pub(crate) static PUBSUB_POOL: LazyLock<PubSub> = LazyLock::new(PubSub::new);

#[derive(Deserialize)]
pub(crate) struct WsParams {
    pub token: String,
}

pub(crate) async fn ws_handler(
    State(state): State<AppState>,
    Extension(ip): Extension<crate::rate_limit::ClientIp>,
    params: Result<Query<WsParams>, QueryRejection>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> impl IntoResponse {
    let ws = match ws {
        Ok(ws) => ws,
        Err(_) => return status(StatusCode::BAD_REQUEST, "invalid websocket upgrade request"),
    };
    let Query(params) = match params {
        Ok(params) => params,
        Err(error) => return status(error.status(), error.body_text()),
    };
    if params.token.len() != 37 {
        return status(StatusCode::BAD_REQUEST, "incorrect token length");
    }
    let auth_state = state.clone();
    let session = match tokio::task::spawn_blocking(move || {
        token::authenticate_session(&auth_state, &params.token)
    })
    .await
    {
        Ok(Ok(user)) => user,
        Ok(Err(error)) => return error.into_response(),
        Err(error) => {
            eprintln!("Token authentication task failed: {error}");
            return status(StatusCode::INTERNAL_SERVER_ERROR, "internal server error");
        }
    };

    ws.max_message_size(16 * 1024)
        .max_frame_size(16 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, state, session, ip.0))
        .into_response()
}

struct SubscriberGuard(SubscriberId);

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        PUBSUB_POOL.remove_subscriber(self.0);
    }
}

fn error_event(event: &str, id: &str, message: &str) -> Event {
    Event {
        event: event.into(),
        id: id.into(),
        data: Data::from([("fakywaky".into(), Value::String(message.to_lowercase()))]),
    }
}

async fn dispatch(
    state: AppState,
    subscriber: SubscriberId,
    policy: &mut subscription::SubscriptionState,
    message: &[u8],
) -> Event {
    let request: serde_json::Value = match serde_json::from_slice(message) {
        Ok(request) => request,
        Err(_) => return error_event("generic_err", "", "invalid JSON event"),
    };
    let id = request
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !valid_event_id(id) {
        return error_event(
            "generic_err",
            "",
            "id must contain 1-64 ASCII alphanumeric characters",
        );
    }
    let action = match request.get("event").and_then(serde_json::Value::as_str) {
        Some(action @ ("subscribe" | "unsubscribe")) => action,
        _ => return error_event("generic_err", id, "unknown or invalid event"),
    };
    let data = request
        .get("data")
        .cloned()
        .and_then(|value| serde_json::from_value::<Data>(value).ok());
    let Some(data) = data else {
        return error_event(
            &format!("{action}_err"),
            id,
            "data must be an object containing strings or nested objects",
        );
    };
    let result = if action == "subscribe" {
        subscription::subscribe(state, &PUBSUB_POOL, subscriber, policy, &data).await
    } else {
        subscription::unsubscribe(&PUBSUB_POOL, subscriber, policy, &data)
    };
    match result {
        Ok(()) => Event {
            event: format!("{action}_ok"),
            id: id.into(),
            data: Data::new(),
        },
        Err(message) => error_event(&format!("{action}_err"), id, &message),
    }
}

async fn close_session(socket: &mut WebSocket) {
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.send(Message::Close(Some(sockudo_ws::error::CloseReason {
            code: 1008,
            reason: "session ended".into(),
        }))),
    )
    .await;
}

enum Outbound {
    Message(Message),
    Publication(pubsub::Publication),
    Continue,
    Stop,
    RateLimited(u64),
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    session: token::AuthenticatedSession,
    ip: std::net::IpAddr,
) {
    let (_session_guard, mut revoked) = sessions::SessionGuard::register(session.key.clone());
    let auth_state = state.clone();
    let session_key = session.key;
    let verified = tokio::task::spawn_blocking(move || {
        token::authenticate_session_key(&auth_state, &session_key)
    })
    .await;
    let session = match verified {
        Ok(Ok(session))
            if revoked.try_recv() == Err(tokio::sync::oneshot::error::TryRecvError::Empty) =>
        {
            session
        }
        _ => {
            close_session(&mut socket).await;
            return;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let delay = std::time::Duration::from_secs(session.expires_at).saturating_sub(now);
    let expiration = tokio::time::sleep(delay);
    tokio::pin!(expiration);
    let (subscriber, mut receiver, mut overloaded) = PUBSUB_POOL.create_subscriber();
    let subscriber = SubscriberGuard(subscriber);
    PUBSUB_POOL.subscribe(subscriber.0, &format!("messages/{}", session.user.user_id));
    let mut policy = subscription::SubscriptionState::default();
    loop {
        let next = tokio::select! {
            biased;
            _ = &mut overloaded => { break; }
            _ = &mut revoked => { close_session(&mut socket).await; break; }
            _ = &mut expiration => { close_session(&mut socket).await; break; }
            next = async {
                tokio::select! {
                    incoming = socket.recv() => match incoming {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(seconds) = state.limits.check_socket(&state.config, ip, &session.user.user_id) {
                                return Outbound::RateLimited(seconds);
                            }
                            let reply = dispatch(state.clone(), subscriber.0, &mut policy, text.as_ref()).await;
                            match serde_json::to_string(&reply) {
                                Ok(json) => Outbound::Message(Message::text(json)),
                                Err(error) => {
                                    eprintln!("WebSocket event serialization failed: {error}");
                                    Outbound::Stop
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => Outbound::Stop,
                        Some(Ok(Message::Binary(_))) => match state.limits.check_socket(&state.config, ip, &session.user.user_id) {
                            Ok(()) => Outbound::Continue,
                            Err(seconds) => Outbound::RateLimited(seconds),
                        },
                        Some(Ok(_)) => Outbound::Continue,
                        Some(Err(error)) => {
                            eprintln!("WebSocket error: {error}");
                            Outbound::Stop
                        }
                    },
                    publication = receiver.recv() => match publication {
                        Some(publication) => Outbound::Publication(publication),
                        None => Outbound::Stop,
                    },
                }
            } => next,
        };
        let (outbound, _budget) = match next {
            Outbound::Message(message) => (message, None),
            Outbound::Publication(publication) => {
                let (message, budget) = publication.into_parts();
                (message, Some(budget))
            }
            Outbound::Continue => continue,
            Outbound::Stop => break,
            Outbound::RateLimited(seconds) => {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    socket.send(Message::Close(Some(sockudo_ws::error::CloseReason {
                        code: 1008,
                        reason: format!("rate limit exceeded; retry after {seconds}s"),
                    }))),
                )
                .await;
                break;
            }
        };
        let sent = tokio::select! {
            biased;
            _ = &mut overloaded => { break; }
            _ = &mut revoked => { close_session(&mut socket).await; break; }
            _ = &mut expiration => { close_session(&mut socket).await; break; }
            sent = tokio::time::timeout(std::time::Duration::from_secs(5), socket.send(outbound)) => sent,
        };
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("WebSocket send failed: {error}");
                break;
            }
            Err(_) => break,
        }
    }
}
use std::sync::LazyLock;
