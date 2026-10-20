use crate::socket::PubSub;
use axum::{
    Json,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use sockudo_ws::Message;

use crate::{
    AppState,
    socket::PUBSUB_POOL,
    spa::{data, error_response},
    token::{self, AuthenticatedUser, TokenError},
    types::{Data, Event, Value, valid_uuid},
};

#[derive(Debug)]
enum SendError {
    Invalid(&'static str),
    NotFound,
    Offline,
    Authentication(TokenError),
    Internal(String),
}

impl SendError {
    fn into_response(self, id: &str) -> Response {
        let (code, message) = match self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, message.into()),
            Self::NotFound => (StatusCode::NOT_FOUND, "user does not exist".into()),
            Self::Offline => (StatusCode::CONFLICT, "the user is not online".into()),
            Self::Authentication(TokenError::Unauthorized(message)) => {
                (StatusCode::UNAUTHORIZED, message.into())
            }
            Self::Authentication(TokenError::Request(code, message)) => (code, message),
            Self::Authentication(error @ TokenError::RateLimited(_)) => {
                return error.into_response();
            }
            Self::Internal(error) | Self::Authentication(TokenError::Internal(error)) => {
                eprintln!("Send error: {error}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
        };
        error_response(code, "send_err", id, &message)
    }
}

fn internal(error: impl std::fmt::Display) -> SendError {
    SendError::Internal(error.to_string())
}

struct SendMessage {
    to: String,
    content: Value,
}

impl SendMessage {
    fn parse(data: &Data) -> Result<Self, SendError> {
        let to = match data.get("to") {
            Some(Value::String(to)) if valid_uuid(to) => to.clone(),
            _ => {
                return Err(SendError::Invalid("to must be an account ID"));
            }
        };
        let content = data
            .get("content")
            .cloned()
            .ok_or(SendError::Invalid("content is required"))?;
        Ok(Self { to, content })
    }
}

pub(crate) async fn handle(
    state: AppState,
    headers: HeaderMap,
    id: &str,
    input: Option<serde_json::Value>,
) -> Response {
    let input = input.and_then(|value| serde_json::from_value::<Data>(value).ok());
    let input = match input.as_ref().map(SendMessage::parse) {
        Some(Ok(input)) => input,
        Some(Err(error)) => return error.into_response(id),
        None => {
            return SendError::Invalid(
                "data must be an object containing strings or nested objects",
            )
            .into_response(id);
        }
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.split_once(' '))
        .filter(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer") && token.len() == 37)
        .map(|(_, token)| token.to_owned());
    let Some(bearer) = bearer else {
        return SendError::Authentication(TokenError::Unauthorized(
            "missing or invalid bearer token",
        ))
        .into_response(id);
    };
    let request_id = id.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let user = token::authenticate_user(&state, &bearer).map_err(SendError::Authentication)?;
        send_message(&state, &user, input, &request_id, &PUBSUB_POOL)
    })
    .await;
    match result {
        Ok(Ok(reply)) => (StatusCode::OK, Json(reply)).into_response(),
        Ok(Err(error)) => error.into_response(id),
        Err(error) => internal(error).into_response(id),
    }
}

fn send_message(
    state: &AppState,
    sender: &AuthenticatedUser,
    input: SendMessage,
    id: &str,
    pool: &PubSub,
) -> Result<Event, SendError> {
    let recipient_id = {
        let (env, database) = (&state.environment, &state.database);
        let txn = env.read_txn().map_err(internal)?;
        let key = format!("user:{}", input.to);
        database
            .get(&txn, key.as_bytes())
            .map_err(internal)?
            .ok_or(SendError::NotFound)?;
        input.to
    };
    let mut payload = data(&[("from", &sender.user_id)]);
    payload.insert("content".into(), input.content);
    let received = Event {
        event: "send_recv".into(),
        id: id.into(),
        data: payload,
    };
    let publication = serde_json::to_string(&received).map_err(internal)?;
    if pool.publish(
        &format!("messages/{recipient_id}"),
        Message::text(publication),
    ) == 0
    {
        return Err(SendError::Offline);
    }
    Ok(Event {
        event: "send_ok".into(),
        id: id.into(),
        data: Data::new(),
    })
}
