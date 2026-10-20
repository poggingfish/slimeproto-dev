use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use ed25519_dalek::{Signer, SigningKey};
use heed::{Database, Env, RoTxn, types::Bytes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{
    AppState, format_uuid, registration,
    token::{self, TokenError},
    types::{Collection, Document, UserRecord, valid_uuid},
};

const BACKUP_FORMAT: &str = "slimeproto-identity-v1";
const BACKUP_KDF: &str = "argon2id-v19-m19456-t2-p1";
const BACKUP_CIPHER: &str = "xchacha20poly1305";

pub(crate) fn internal(error: impl std::fmt::Display) -> TokenError {
    TokenError::Internal(error.to_string())
}

fn invalid(message: &str) -> TokenError {
    TokenError::Request(StatusCode::BAD_REQUEST, message.into())
}

pub(crate) fn initialize_schema(
    env: &Env,
    database: Database<Bytes, Bytes>,
) -> Result<(), Box<dyn std::error::Error>> {
    const SCHEMA: &[u8] = b"schema:identity";
    let mut txn = env.write_txn()?;
    match database.get(&txn, SCHEMA)? {
        Some(b"1") => return Ok(()),
        Some(_) => return Err("unsupported identity schema".into()),
        None if database.len(&txn)? != 0 => {
            return Err(
                "legacy database is incompatible; select an empty SLIMEPROTO_DATA_PATH".into(),
            );
        }
        None => {}
    }
    database.put(&mut txn, SCHEMA, b"1")?;
    txn.commit()?;
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct PublicIdentity {
    pub account_id: String,
    pub algorithm: &'static str,
    pub public_key: String,
}

pub(crate) fn account_id(public_key: &[u8; 32]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"slimeproto:account:v1:");
    hash.update(public_key);
    let mut bytes: [u8; 16] = hash.finalize()[..16].try_into().expect("SHA-256 length");
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&uuid::Uuid::from_bytes(bytes))
}

pub(crate) fn public_identity(seed: &[u8; 32]) -> PublicIdentity {
    let public_key = SigningKey::from_bytes(seed).verifying_key().to_bytes();
    PublicIdentity {
        account_id: account_id(&public_key),
        algorithm: "ed25519",
        public_key: hex(&public_key),
    }
}

pub(crate) fn load_user(
    state: &AppState,
    txn: &RoTxn<'_>,
    id: &str,
) -> Result<UserRecord, TokenError> {
    let encoded = state
        .database
        .get(txn, format!("user:{id}").as_bytes())
        .map_err(crate::storage::database_error)?
        .ok_or(TokenError::Unauthorized("incorrect account ID or password"))?;
    let user: UserRecord = serde_json::from_slice(encoded).map_err(internal)?;
    if public_identity(&user.signing_seed).account_id != id {
        return Err(internal("account key does not match its ID"));
    }
    Ok(user)
}

pub(crate) fn validate_password(password: &str) -> Result<(), TokenError> {
    if password.len() > 1024 || password.chars().count() < 10 {
        return Err(invalid(
            "password must contain at least 10 characters and at most 1024 UTF-8 bytes",
        ));
    }
    Ok(())
}

pub(crate) fn hash_password(password: &str) -> Result<String, TokenError> {
    Ok(Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(internal)?
        .to_string())
}

pub(crate) fn verify_password(user: &UserRecord, password: &str) -> Result<(), TokenError> {
    if password.len() > 1024 {
        return Err(TokenError::Unauthorized("incorrect account ID or password"));
    }
    let hash = PasswordHash::new(&user.password_hash).map_err(internal)?;
    match Argon2::default().verify_password(password.as_bytes(), &hash) {
        Ok(()) => Ok(()),
        Err(argon2::password_hash::Error::PasswordInvalid) => {
            Err(TokenError::Unauthorized("incorrect account ID or password"))
        }
        Err(error) => Err(internal(error)),
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[(byte >> 4) as usize] as char,
                DIGITS[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}

fn unhex<const N: usize>(value: &str) -> Result<[u8; N], TokenError> {
    if value.len() != N * 2 {
        return Err(invalid("invalid backup encoding"));
    }
    let mut bytes = [0; N];
    for (output, pair) in bytes.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let digit = |byte| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(invalid("invalid backup encoding")),
        };
        *output = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Ok(bytes)
}

pub(crate) fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        value => value,
    }
}

fn sign(
    state: &AppState,
    txn: &RoTxn<'_>,
    author: &str,
    domain: &[u8],
    mut payload: serde_json::Value,
) -> Result<String, TokenError> {
    let object = payload
        .as_object_mut()
        .ok_or_else(|| internal("invalid signing payload"))?;
    object.remove("signature");
    object.remove("seq");
    let mut message = domain.to_vec();
    message.extend(serde_json::to_vec(&canonical_json(payload)).map_err(internal)?);
    let user = load_user(state, txn, author)?;
    Ok(hex(&SigningKey::from_bytes(&user.signing_seed)
        .sign(&message)
        .to_bytes()))
}

pub(crate) fn sign_document(
    state: &AppState,
    txn: &RoTxn<'_>,
    document: &Document,
) -> Result<String, TokenError> {
    sign(
        state,
        txn,
        &document.author,
        b"slimeproto:document:v1:\0",
        serde_json::to_value(document).map_err(internal)?,
    )
}

pub(crate) fn sign_collection(
    state: &AppState,
    txn: &RoTxn<'_>,
    collection: &Collection,
) -> Result<String, TokenError> {
    sign(
        state,
        txn,
        &collection.author,
        b"slimeproto:collection:v1:\0",
        serde_json::to_value(collection).map_err(internal)?,
    )
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Backup {
    format: String,
    account_id: String,
    public_key: String,
    kdf: String,
    cipher: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

impl Backup {
    fn aad(&self) -> Vec<u8> {
        format!(
            "slimeproto:identity-backup:v1:{}:{}",
            self.account_id, self.public_key
        )
        .into_bytes()
    }
}

fn backup_key(password: &str, salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>, TokenError> {
    let mut key = Zeroizing::new([0; 32]);
    let params = Params::new(19456, 2, 1, Some(32)).map_err(internal)?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(internal)?;
    Ok(key)
}

fn encrypt_backup(seed: &[u8; 32], password: &str) -> Result<Backup, TokenError> {
    let public = public_identity(seed);
    let mut salt = [0; 16];
    let mut nonce = [0; 24];
    getrandom::fill(&mut salt).map_err(internal)?;
    getrandom::fill(&mut nonce).map_err(internal)?;
    let mut backup = Backup {
        format: BACKUP_FORMAT.into(),
        account_id: public.account_id,
        public_key: public.public_key,
        kdf: BACKUP_KDF.into(),
        cipher: BACKUP_CIPHER.into(),
        salt: hex(&salt),
        nonce: hex(&nonce),
        ciphertext: String::new(),
    };
    let key = backup_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(internal)?;
    backup.ciphertext = hex(&cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: seed,
                aad: &backup.aad(),
            },
        )
        .map_err(internal)?);
    Ok(backup)
}

fn decrypt_backup(backup: &Backup, password: &str) -> Result<Zeroizing<[u8; 32]>, TokenError> {
    if backup.format != BACKUP_FORMAT
        || backup.kdf != BACKUP_KDF
        || backup.cipher != BACKUP_CIPHER
        || !valid_uuid(&backup.account_id)
    {
        return Err(invalid("unsupported or invalid identity backup"));
    }
    let salt = unhex::<16>(&backup.salt)?;
    let nonce = unhex::<24>(&backup.nonce)?;
    let ciphertext = unhex::<48>(&backup.ciphertext)?;
    let key = backup_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(internal)?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &backup.aad(),
                },
            )
            .map_err(|_| invalid("incorrect backup password or damaged backup"))?,
    );
    let seed = Zeroizing::new(plaintext.as_slice().try_into().map_err(internal)?);
    let public = public_identity(&seed);
    if public.account_id != backup.account_id || public.public_key != backup.public_key {
        return Err(invalid("backup key does not match its identity"));
    }
    Ok(seed)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExportRequest {
    password: String,
    backup_password: String,
}

pub(crate) async fn export_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Result<Json<ExportRequest>, JsonRejection>,
) -> Response {
    let result = async {
        let Json(request) =
            request.map_err(|error| TokenError::Request(error.status(), error.body_text()))?;
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, value)| scheme.eq_ignore_ascii_case("bearer") && value.len() == 37)
            .map(|(_, token)| token.to_owned())
            .ok_or(TokenError::Unauthorized("missing or invalid bearer token"))?;
        let backup = crate::password_work::run(move || {
            validate_password(&request.backup_password)?;
            let session = token::authenticate_user(&state, &bearer)?;
            let user = {
                let txn = state
                    .environment
                    .read_txn()
                    .map_err(crate::storage::database_error)?;
                load_user(&state, &txn, &session.user_id)?
            };
            verify_password(&user, &request.password)?;
            token::authenticate_user(&state, &bearer)?;
            encrypt_backup(&user.signing_seed, &request.backup_password)
        })
        .await?;
        Ok::<_, TokenError>(Json(backup).into_response())
    }
    .await;
    no_store(match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportRequest {
    backup: Backup,
    backup_password: String,
    password: String,
    registration_key: Option<String>,
}

pub(crate) async fn import_handler(
    State(state): State<AppState>,
    request: Result<Json<ImportRequest>, JsonRejection>,
) -> Response {
    let result = async {
        let Json(request) =
            request.map_err(|error| TokenError::Request(error.status(), error.body_text()))?;
        let public = crate::password_work::run(move || {
            state
                .config
                .check_registration(request.registration_key.as_deref())?;
            validate_password(&request.password)?;
            validate_password(&request.backup_password)?;
            let seed = decrypt_backup(&request.backup, &request.backup_password)?;
            state
                .limits
                .check_user(&state.config, &public_identity(&seed).account_id)?;
            registration::store_account(&state, &seed, &request.password)
        })
        .await?;
        Ok::<_, TokenError>((StatusCode::CREATED, Json(public)).into_response())
    }
    .await;
    no_store(match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    })
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
