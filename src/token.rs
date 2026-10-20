use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{AppState, format_uuid, identity, types::valid_uuid};

mod expiry;
pub(crate) use expiry::{initialize_sessions, start_cleanup};

const TOKEN_LIFETIME_SECONDS: u64 = 24 * 60 * 60;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenRequest {
    account_id: String,
    password: String,
}

#[derive(Serialize)]
struct TokenResponse {
    token: String,
    expires_at: u64,
}

#[derive(Deserialize, Serialize)]
struct Session {
    user_id: String,
    expires_at: u64,
}

#[derive(Debug)]
pub(crate) enum TokenError {
    Unauthorized(&'static str),
    Request(StatusCode, String),
    Internal(String),
    RateLimited(u64),
}

impl IntoResponse for TokenError {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Unauthorized(message) => crate::status(StatusCode::UNAUTHORIZED, message),
            Self::Request(code, message) => {
                let mut response = crate::status(code, message);
                if code == StatusCode::SERVICE_UNAVAILABLE {
                    response
                        .headers_mut()
                        .insert(header::RETRY_AFTER, "1".parse().unwrap());
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                }
                response
            }
            Self::RateLimited(seconds) => crate::rate_limit::response(seconds),
            Self::Internal(error) => {
                eprintln!("Token error: {error}");
                crate::status(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
            }
        }
    }
}

fn internal(error: impl std::fmt::Display) -> TokenError {
    TokenError::Internal(error.to_string())
}

fn now() -> Result<u64, TokenError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_secs())
}

fn token_key(token: &str) -> Vec<u8> {
    let mut key = b"token:".to_vec();
    key.extend_from_slice(&Sha256::digest(token.as_bytes()));
    key
}

pub(crate) async fn token_handler(
    State(state): State<AppState>,
    request: Result<Json<TokenRequest>, JsonRejection>,
) -> Result<axum::response::Response, TokenError> {
    let Json(request) =
        request.map_err(|error| TokenError::Request(error.status(), error.body_text()))?;
    let response = crate::password_work::run(move || issue_token(&state, request)).await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response())
}

fn issue_token(state: &AppState, request: TokenRequest) -> Result<TokenResponse, TokenError> {
    if !valid_uuid(&request.account_id) || request.password.len() > 1024 {
        return Err(TokenError::Unauthorized("incorrect account ID or password"));
    }
    let (env, database) = (&state.environment, &state.database);
    let user = {
        let txn = env.read_txn().map_err(crate::storage::database_error)?;
        identity::load_user(state, &txn, &request.account_id)?
    };
    identity::verify_password(&user, &request.password)?;
    state
        .limits
        .check_user(&state.config, &request.account_id)?;

    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(internal)?;
    let token = format_uuid(&Uuid::from_bytes(bytes));
    let expires_at = now()? + TOKEN_LIFETIME_SECONDS;
    let session = Session {
        user_id: request.account_id,
        expires_at,
    };
    let encoded = serde_json::to_vec(&session).map_err(internal)?;
    let key = token_key(&token);
    let mut txn = env.write_txn().map_err(crate::storage::database_error)?;
    if database
        .get(&txn, &key)
        .map_err(crate::storage::database_error)?
        .is_some()
    {
        return Err(internal("random token collision"));
    }
    database
        .put(&mut txn, &key, &encoded)
        .map_err(crate::storage::database_error)?;
    database
        .put(&mut txn, &expiry::key(expires_at, &key), b"")
        .map_err(crate::storage::database_error)?;
    txn.commit().map_err(crate::storage::database_error)?;

    Ok(TokenResponse { token, expires_at })
}

pub(crate) struct AuthenticatedUser {
    pub user_id: String,
}

pub(crate) fn authenticate_user(
    state: &AppState,
    token: &str,
) -> Result<AuthenticatedUser, TokenError> {
    Ok(authenticate_session_key(state, &token_key(token))?.user)
}

pub(crate) struct AuthenticatedSession {
    pub user: AuthenticatedUser,
    pub key: Vec<u8>,
    pub expires_at: u64,
}

pub(crate) fn authenticate_session(
    state: &AppState,
    token: &str,
) -> Result<AuthenticatedSession, TokenError> {
    authenticate_session_key(state, &token_key(token))
}

pub(crate) fn authenticate_session_key(
    state: &AppState,
    key: &[u8],
) -> Result<AuthenticatedSession, TokenError> {
    let txn = state
        .environment
        .read_txn()
        .map_err(crate::storage::database_error)?;
    authenticate_session_in_txn(state, &txn, key)
}

pub(crate) fn authenticate_user_in_txn(
    state: &AppState,
    txn: &heed::RoTxn<'_>,
    token: &str,
) -> Result<AuthenticatedUser, TokenError> {
    Ok(authenticate_session_in_txn(state, txn, &token_key(token))?.user)
}

fn authenticate_session_in_txn(
    state: &AppState,
    txn: &heed::RoTxn<'_>,
    key: &[u8],
) -> Result<AuthenticatedSession, TokenError> {
    let invalid_token = || TokenError::Unauthorized("incorrect token");
    let database = &state.database;
    let encoded = database
        .get(txn, key)
        .map_err(crate::storage::database_error)?
        .ok_or_else(invalid_token)?;
    let session: Session = serde_json::from_slice(encoded).map_err(internal)?;
    if session.expires_at <= now()? {
        return Err(invalid_token());
    }
    identity::load_user(state, txn, &session.user_id)?;
    Ok(AuthenticatedSession {
        user: AuthenticatedUser {
            user_id: session.user_id,
        },
        key: key.to_vec(),
        expires_at: session.expires_at,
    })
}

pub(crate) fn revoke_token(state: &AppState, token: &str) -> Result<(), TokenError> {
    let key = token_key(token);
    let (env, database) = (&state.environment, &state.database);
    let mut txn = env.write_txn().map_err(crate::storage::database_error)?;
    if let Some(encoded) = database
        .get(&txn, &key)
        .map_err(crate::storage::database_error)?
    {
        let session: Session = serde_json::from_slice(encoded).map_err(internal)?;
        if session.expires_at > now()?
            && database
                .get(&txn, format!("user:{}", session.user_id).as_bytes())
                .map_err(crate::storage::database_error)?
                .is_none()
        {
            return Err(TokenError::Unauthorized("incorrect token"));
        }
        database
            .delete(&mut txn, &expiry::key(session.expires_at, &key))
            .map_err(crate::storage::database_error)?;
        database
            .delete(&mut txn, &key)
            .map_err(crate::storage::database_error)?;
    }
    txn.commit().map_err(crate::storage::database_error)?;
    crate::socket::revoke_session(&key);
    Ok(())
}
