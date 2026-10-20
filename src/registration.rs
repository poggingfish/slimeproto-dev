use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::Deserialize;

use crate::{
    AppState,
    identity::{self, PublicIdentity},
    token::TokenError,
    types::UserRecord,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegisterRequest {
    password: String,
    registration_key: Option<String>,
}

pub(crate) async fn register_handler(
    State(state): State<AppState>,
    request: Result<Json<RegisterRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<PublicIdentity>), TokenError> {
    let Json(request) =
        request.map_err(|error| TokenError::Request(error.status(), error.body_text()))?;
    let response = crate::password_work::run(move || {
        state
            .config
            .check_registration(request.registration_key.as_deref())?;
        identity::validate_password(&request.password)?;
        let mut seed = zeroize::Zeroizing::new([0; 32]);
        getrandom::fill(seed.as_mut()).map_err(identity::internal)?;
        store_account(&state, &seed, &request.password)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(response)))
}

pub(crate) fn store_account(
    state: &AppState,
    seed: &[u8; 32],
    password: &str,
) -> Result<PublicIdentity, TokenError> {
    let public = identity::public_identity(seed);
    let record = UserRecord {
        password_hash: identity::hash_password(password)?,
        signing_seed: *seed,
    };
    let encoded = zeroize::Zeroizing::new(serde_json::to_vec(&record).map_err(identity::internal)?);
    let key = format!("user:{}", public.account_id);
    let mut txn = state
        .environment
        .write_txn()
        .map_err(crate::storage::database_error)?;
    if state
        .database
        .get(&txn, key.as_bytes())
        .map_err(crate::storage::database_error)?
        .is_some()
    {
        return Err(TokenError::Request(
            StatusCode::CONFLICT,
            "account already exists".into(),
        ));
    }
    state
        .database
        .put(&mut txn, key.as_bytes(), &encoded)
        .map_err(crate::storage::database_error)?;
    txn.commit().map_err(crate::storage::database_error)?;
    Ok(public)
}
