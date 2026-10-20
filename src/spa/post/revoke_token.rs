use crate::spa::error_response;
use crate::{
    AppState,
    token::{self, TokenError},
    types::{Data, Event, valid_uuid},
};
use axum::{
    Json,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};

pub(crate) async fn handle(
    state: AppState,
    headers: HeaderMap,
    id: &str,
    input: Option<serde_json::Value>,
) -> Response {
    let result = handle_inner(state, headers, input).await;
    let mut response = match result {
        Ok(()) => Json(Event {
            event: "revoke_token_ok".into(),
            id: id.into(),
            data: Data::new(),
        })
        .into_response(),
        Err(TokenError::Unauthorized(message)) => {
            error_response(StatusCode::UNAUTHORIZED, "revoke_token_err", id, message)
        }
        Err(TokenError::Request(code, message)) => {
            error_response(code, "revoke_token_err", id, &message)
        }
        Err(error @ TokenError::RateLimited(_)) => error.into_response(),
        Err(TokenError::Internal(error)) => {
            eprintln!("Token revocation failed: {error}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "revoke_token_err",
                id,
                "internal server error",
            )
        }
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn handle_inner(
    state: AppState,
    headers: HeaderMap,
    input: Option<serde_json::Value>,
) -> Result<(), TokenError> {
    if !input
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .is_some_and(|data| data.is_empty())
    {
        return Err(TokenError::Request(
            StatusCode::BAD_REQUEST,
            "data must be an empty object".into(),
        ));
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, value)| scheme.eq_ignore_ascii_case("bearer") && valid_uuid(value))
        .map(|(_, value)| value.to_owned())
        .ok_or(TokenError::Unauthorized("missing or invalid bearer token"))?;
    tokio::task::spawn_blocking(move || token::revoke_token(&state, &bearer))
        .await
        .map_err(|error| TokenError::Internal(error.to_string()))?
}
