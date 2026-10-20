use crate::socket::PubSub;
use axum::{
    Json,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sockudo_ws::Message;
use std::{
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    AppState, generate_uuid_v7,
    socket::PUBSUB_POOL,
    spa::{data, error_response},
    token::{self, AuthenticatedUser, TokenError},
    types::{Collection, Data, Document, Event, MAX_DOCUMENT_REFS, Value, refs_data, valid_uuid},
};

const SEQUENCE_KEY: &[u8] = b"counter:document_seq";
const RETRY_LIFETIME: u64 = 24 * 60 * 60;
const RETRY_EXPIRY_PREFIX: &[u8] = b"idempotency_expiry:";
const RETRY_CLEANUP_BATCH: usize = 256;

#[derive(Debug)]
enum ActionError {
    Invalid(&'static str),
    NotFound,
    ReferenceNotFound,
    Conflict,
    IdempotencyConflict,
    Authentication(TokenError),
    Internal(String),
}

impl ActionError {
    fn into_response(self, id: &str) -> Response {
        let (code, message) = match self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, message.into()),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "collection does not exist".to_owned(),
            ),
            Self::ReferenceNotFound => (
                StatusCode::NOT_FOUND,
                "referenced document does not exist".into(),
            ),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "document name already exists in this collection".into(),
            ),
            Self::IdempotencyConflict => (
                StatusCode::CONFLICT,
                "idempotency key was already used with different document data".into(),
            ),
            Self::Authentication(TokenError::Unauthorized(message)) => {
                (StatusCode::UNAUTHORIZED, message.into())
            }
            Self::Authentication(TokenError::Request(code, message)) => (code, message),
            Self::Authentication(error @ TokenError::RateLimited(_)) => {
                return error.into_response();
            }
            Self::Internal(error) | Self::Authentication(TokenError::Internal(error)) => {
                eprintln!("Document creation error: {error}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
        };
        error_response(code, "create_document_err", id, &message)
    }
}

fn database_error(error: heed::Error) -> ActionError {
    ActionError::Authentication(crate::storage::database_error(error))
}

fn internal(error: impl std::fmt::Display) -> ActionError {
    ActionError::Internal(error.to_string())
}

#[derive(Serialize)]
struct CreateDocument {
    collection: String,
    name: Option<String>,
    kind: String,
    content: Data,
    refs: Vec<String>,
}

struct RetryInput {
    key: String,
    fingerprint: [u8; 32],
}

#[derive(Deserialize, Serialize)]
struct RetryRecord {
    fingerprint: [u8; 32],
    uuid: String,
    seq: u64,
    expires_at: u64,
}

fn now() -> Result<u64, ActionError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_secs())
}

fn expiry_key(expires_at: u64, key: &[u8]) -> Vec<u8> {
    let mut expiry = sequence_index(RETRY_EXPIRY_PREFIX, expires_at);
    expiry.extend_from_slice(key);
    expiry
}

fn retry_input(
    headers: &HeaderMap,
    input: &CreateDocument,
) -> Result<Option<RetryInput>, ActionError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .map_err(|_| ActionError::Invalid("invalid Idempotency-Key"))?;
    if values.next().is_some()
        || !(1..=128).contains(&key.len())
        || !key.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(ActionError::Invalid(
            "Idempotency-Key must be 1-128 printable ASCII characters without spaces and supplied once",
        ));
    }
    let canonical = crate::identity::canonical_json(serde_json::to_value(input).map_err(internal)?);
    Ok(Some(RetryInput {
        key: key.into(),
        fingerprint: Sha256::digest(serde_json::to_vec(&canonical).map_err(internal)?).into(),
    }))
}

fn prune_retries(state: &AppState) -> Result<usize, ActionError> {
    let timestamp = now()?;
    let mut txn = state.environment.write_txn().map_err(database_error)?;
    let expired = state
        .database
        .prefix_iter(&txn, RETRY_EXPIRY_PREFIX)
        .map_err(database_error)?
        .take(RETRY_CLEANUP_BATCH)
        .map(|row| {
            let (key, _) = row.map_err(internal)?;
            let end = RETRY_EXPIRY_PREFIX.len() + 8;
            let bytes = key
                .get(RETRY_EXPIRY_PREFIX.len()..end)
                .ok_or_else(|| internal("invalid idempotency expiry index"))?;
            let expires_at = u64::from_be_bytes(bytes.try_into().map_err(internal)?);
            Ok((key.to_vec(), expires_at))
        })
        .take_while(|row| !matches!(row, Ok((_, expiry)) if *expiry > timestamp))
        .collect::<Result<Vec<_>, ActionError>>()?;
    let count = expired.len();
    for (expiry, _) in expired {
        state
            .database
            .delete(&mut txn, &expiry[RETRY_EXPIRY_PREFIX.len() + 8..])
            .map_err(database_error)?;
        state
            .database
            .delete(&mut txn, &expiry)
            .map_err(database_error)?;
    }
    txn.commit().map_err(database_error)?;
    Ok(count)
}

impl CreateDocument {
    fn parse(data: &Data) -> Result<Self, ActionError> {
        let collection = match data.get("collection") {
            Some(Value::String(value)) if crate::names::valid_collection_name(value) => {
                value.clone()
            }
            _ => {
                return Err(ActionError::Invalid(
                    "collection must be a nonblank string of at most 256 bytes",
                ));
            }
        };
        let name = match data.get("name") {
            None => None,
            Some(Value::String(value)) if value.len() <= crate::names::MAX_DOCUMENT_NAME_BYTES => {
                Some(value.clone())
            }
            _ => {
                return Err(ActionError::Invalid(
                    "name must be a string of at most 128 bytes when provided",
                ));
            }
        };
        let kind = match data.get("type") {
            Some(Value::String(value)) if !value.trim().is_empty() && value.len() <= 64 => {
                value.clone()
            }
            _ => {
                return Err(ActionError::Invalid(
                    "type must be a nonblank string of at most 64 bytes",
                ));
            }
        };
        let content = match data.get("content") {
            Some(Value::Object(content)) => content.clone(),
            _ => {
                return Err(ActionError::Invalid(
                    "content must be an object containing strings or nested objects",
                ));
            }
        };
        let mut refs = Vec::new();
        if let Some(value) = data.get("refs") {
            let Value::Object(values) = value else {
                return Err(ActionError::Invalid(
                    "refs must be an object indexed by consecutive numeric keys",
                ));
            };
            if values.len() > MAX_DOCUMENT_REFS {
                return Err(ActionError::Invalid("refs must contain at most 64 UUIDs"));
            }
            let mut seen = std::collections::HashSet::new();
            for index in 0..values.len() {
                let Some(Value::String(uuid)) = values.get(&index.to_string()) else {
                    return Err(ActionError::Invalid(
                        "refs must contain UUID strings under consecutive keys starting at 0",
                    ));
                };
                if !valid_uuid(uuid) {
                    return Err(ActionError::Invalid("invalid reference uuid"));
                }
                if !seen.insert(uuid) {
                    return Err(ActionError::Invalid("duplicate reference uuid"));
                }
                refs.push(uuid.clone());
            }
        }
        Ok(Self {
            collection,
            name,
            kind,
            content,
            refs,
        })
    }
}

pub(crate) async fn handle(
    _state: AppState,
    headers: HeaderMap,
    id: &str,
    input: Option<serde_json::Value>,
) -> Response {
    let input = input.and_then(|value| serde_json::from_value::<Data>(value).ok());
    let input = match input.as_ref().map(CreateDocument::parse) {
        Some(Ok(input)) => input,
        Some(Err(error)) => return error.into_response(id),
        None => {
            return ActionError::Invalid(
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
        return ActionError::Authentication(TokenError::Unauthorized(
            "missing or invalid bearer token",
        ))
        .into_response(id);
    };
    let retry = match retry_input(&headers, &input) {
        Ok(retry) => retry,
        Err(error) => return error.into_response(id),
    };
    let (reply, result) = oneshot::channel();
    let Some(writer) = WRITER.get() else {
        return internal("Document writer unavailable").into_response(id);
    };
    if writer
        .send(WriteRequest {
            bearer,
            input,
            retry,
            id: id.into(),
            reply,
        })
        .await
        .is_err()
    {
        return internal("Document writer stopped").into_response(id);
    }
    match result.await {
        Ok(Ok(reply)) => (StatusCode::CREATED, Json(reply)).into_response(),
        Ok(Err(error)) => error.into_response(id),
        Err(error) => internal(error).into_response(id),
    }
}

fn sequence_index(prefix: &[u8], seq: u64) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn create_document(
    state: &AppState,
    user: &AuthenticatedUser,
    input: CreateDocument,
    id: &str,
    retry: Option<RetryInput>,
    mut txn: heed::RwTxn<'_>,
) -> Result<WriteOutcome, ActionError> {
    let database = &state.database;
    let retry = if let Some(retry) = retry {
        let timestamp = now()?;
        let mut key = format!("idempotency:create_document:{}:", user.user_id).into_bytes();
        key.extend_from_slice(&Sha256::digest(retry.key.as_bytes()));
        if let Some(encoded) = database.get(&txn, &key).map_err(database_error)? {
            let record: RetryRecord = serde_json::from_slice(encoded).map_err(internal)?;
            if record.expires_at > timestamp {
                if record.fingerprint != retry.fingerprint {
                    return Err(ActionError::IdempotencyConflict);
                }
                return Ok(WriteOutcome::Replayed(Event {
                    event: "create_document_ok".into(),
                    id: id.into(),
                    data: data(&[("uuid", &record.uuid), ("seq", &record.seq.to_string())]),
                }));
            }
            database
                .delete(&mut txn, &expiry_key(record.expires_at, &key))
                .map_err(database_error)?;
        }
        Some((key, retry.fingerprint, timestamp + RETRY_LIFETIME))
    } else {
        None
    };
    let collection_name_key =
        crate::names::collection_key(&user.user_id, &input.collection).map_err(internal)?;
    let collection_uuid = database
        .get(&txn, &collection_name_key)
        .map_err(database_error)?
        .ok_or(ActionError::NotFound)?;
    let collection_uuid = std::str::from_utf8(collection_uuid)
        .map_err(internal)?
        .to_owned();
    let collection_key = format!("collection:{collection_uuid}");
    let encoded = database
        .get(&txn, collection_key.as_bytes())
        .map_err(database_error)?
        .ok_or_else(|| internal("Collection name index references a missing collection"))?;
    let collection: Collection = serde_json::from_slice(encoded).map_err(internal)?;
    if collection.uuid != collection_uuid
        || collection.author != user.user_id
        || collection.name != input.collection
    {
        return Err(internal("collection name index collision or corruption"));
    }

    let name_key = input
        .name
        .as_ref()
        .map(|name| crate::names::document_key(&collection_uuid, name).map_err(internal))
        .transpose()?;
    if let Some(key) = &name_key
        && let Some(uuid) = database.get(&txn, key).map_err(database_error)?
    {
        let uuid = std::str::from_utf8(uuid).map_err(internal)?;
        let encoded = database
            .get(&txn, format!("document:{uuid}").as_bytes())
            .map_err(database_error)?
            .ok_or_else(|| internal("document name index references a missing document"))?;
        let existing: Document = serde_json::from_slice(encoded).map_err(internal)?;
        if existing.uuid != uuid
            || existing.collection != collection_uuid
            || existing.name != input.name
        {
            return Err(internal("document name index collision or corruption"));
        }
        return Err(ActionError::Conflict);
    }
    for reference in &input.refs {
        let encoded = database
            .get(&txn, format!("document:{reference}").as_bytes())
            .map_err(database_error)?
            .ok_or(ActionError::ReferenceNotFound)?;
        let target: Document = serde_json::from_slice(encoded).map_err(internal)?;
        let encoded = database
            .get(&txn, format!("collection:{}", target.collection).as_bytes())
            .map_err(database_error)?
            .ok_or_else(|| internal("referenced document collection is missing"))?;
        let target_collection: Collection = serde_json::from_slice(encoded).map_err(internal)?;
        if target.uuid != *reference || target_collection.uuid != target.collection {
            return Err(internal("inconsistent referenced document"));
        }
        if !target_collection.listed {
            return Err(ActionError::Invalid(
                "referenced documents must belong to listed collections",
            ));
        }
    }
    let previous = match database.get(&txn, SEQUENCE_KEY).map_err(database_error)? {
        Some(bytes) => u64::from_be_bytes(bytes.try_into().map_err(internal)?),
        None => 0,
    };
    let seq = previous
        .checked_add(1)
        .ok_or_else(|| internal("Document sequence exhausted"))?;
    let mut document = Document {
        seq,
        uuid: generate_uuid_v7(),
        collection: collection_uuid.clone(),
        author: user.user_id.clone(),
        signature: String::new(),
        r#type: input.kind,
        name: input.name,
        content: input.content,
        refs: input.refs,
    };
    document.signature = crate::identity::sign_document(state, &txn, &document)
        .map_err(ActionError::Authentication)?;
    let seq_string = seq.to_string();
    let mut metadata = data(&[
        ("author", &user.user_id),
        ("seq", &seq_string),
        ("type", &document.r#type),
        ("uuid", &document.uuid),
        ("collection", &collection_uuid),
        ("collection_name", &collection.name),
        ("signature", &document.signature),
    ]);
    if let Some(name) = &document.name {
        metadata.insert("name".into(), Value::String(name.clone()));
    }
    metadata.insert("refs".into(), Value::Object(refs_data(&document.refs)));
    let received = Event {
        event: "create_document_recv".into(),
        id: id.into(),
        data: metadata,
    };
    let publication = serde_json::to_string(&received).map_err(internal)?;
    let document_key = format!("document:{}", document.uuid);
    let encoded = serde_json::to_vec(&document).map_err(internal)?;
    crate::storage::charge(state, &mut txn, &user.user_id, encoded.len())
        .map_err(ActionError::Authentication)?;
    if database
        .get(&txn, document_key.as_bytes())
        .map_err(database_error)?
        .is_some()
    {
        return Err(internal("Document UUID collision"));
    }
    database
        .put(&mut txn, document_key.as_bytes(), &encoded)
        .map_err(database_error)?;
    database
        .put(&mut txn, SEQUENCE_KEY, &seq.to_be_bytes())
        .map_err(database_error)?;
    database
        .put(
            &mut txn,
            &sequence_index(b"document_seq:", seq),
            document.uuid.as_bytes(),
        )
        .map_err(database_error)?;
    let collection_index = format!("collection_document:{collection_uuid}:");
    database
        .put(
            &mut txn,
            &sequence_index(collection_index.as_bytes(), seq),
            document.uuid.as_bytes(),
        )
        .map_err(database_error)?;
    if let Some(key) = name_key {
        database
            .put(&mut txn, &key, document.uuid.as_bytes())
            .map_err(database_error)?;
    }
    if collection.listed {
        for target in &document.refs {
            let prefix = format!("document_referrer:{target}:");
            database
                .put(
                    &mut txn,
                    &sequence_index(prefix.as_bytes(), seq),
                    document.uuid.as_bytes(),
                )
                .map_err(database_error)?;
        }
    }
    if collection.listed {
        database
            .put(
                &mut txn,
                &sequence_index(b"public_document_seq:", seq),
                document.uuid.as_bytes(),
            )
            .map_err(database_error)?;
        let user_index = format!("listed_document:{}:", user.user_id);
        database
            .put(
                &mut txn,
                &sequence_index(user_index.as_bytes(), seq),
                document.uuid.as_bytes(),
            )
            .map_err(database_error)?;
    }
    if let Some((key, fingerprint, expires_at)) = retry {
        let record = RetryRecord {
            fingerprint,
            uuid: document.uuid.clone(),
            seq,
            expires_at,
        };
        database
            .put(
                &mut txn,
                &key,
                &serde_json::to_vec(&record).map_err(internal)?,
            )
            .map_err(database_error)?;
        database
            .put(&mut txn, &expiry_key(expires_at, &key), b"")
            .map_err(database_error)?;
    }
    txn.commit().map_err(database_error)?;

    Ok(WriteOutcome::Created(CommittedDocument {
        publication,
        collection_uuid,
        listed: collection.listed,
        user_id: user.user_id.clone(),
        reply: Event {
            event: "create_document_ok".into(),
            id: id.into(),
            data: data(&[("uuid", &document.uuid), ("seq", &seq_string)]),
        },
    }))
}

static WRITER: OnceLock<mpsc::Sender<WriteRequest>> = OnceLock::new();
const MAX_BATCH: usize = 64;
const BATCH_WINDOW: Duration = Duration::from_micros(200);

struct WriteRequest {
    bearer: String,
    input: CreateDocument,
    retry: Option<RetryInput>,
    id: String,
    reply: oneshot::Sender<Result<Event, ActionError>>,
}

enum WriteOutcome {
    Created(CommittedDocument),
    Replayed(Event),
}

impl WriteOutcome {
    fn publish(self, pool: &PubSub) -> Event {
        match self {
            Self::Created(document) => document.publish(pool),
            Self::Replayed(reply) => reply,
        }
    }
}

struct CommittedDocument {
    reply: Event,
    publication: String,
    collection_uuid: String,
    user_id: String,
    listed: bool,
}

impl CommittedDocument {
    fn publish(self, pool: &PubSub) -> Event {
        pool.publish(
            &format!("collection/{}", self.collection_uuid),
            Message::text(self.publication.clone()),
        );
        if self.listed {
            pool.publish("firehose", Message::text(self.publication.clone()));
            pool.publish(
                &format!("user/{}", self.user_id),
                Message::text(self.publication),
            );
        }
        self.reply
    }
}

pub(crate) fn start_writer(state: AppState) -> std::io::Result<()> {
    let (sender, mut receiver) = mpsc::channel::<WriteRequest>(256);
    let maintenance_state = state.clone();
    std::thread::Builder::new()
        .name("document-writer".into())
        .spawn(move || {
            while let Some(first) = receiver.blocking_recv() {
                let mut batch = vec![first];
                std::thread::sleep(BATCH_WINDOW);
                while batch.len() < MAX_BATCH {
                    match receiver.try_recv() {
                        Ok(request) => batch.push(request),
                        Err(_) => break,
                    }
                }
                let mut parent = match state.environment.write_txn() {
                    Ok(txn) => txn,
                    Err(error) => {
                        for request in batch {
                            let _ = request.reply.send(Err(ActionError::Authentication(
                                crate::storage::database_error_ref(&error),
                            )));
                        }
                        continue;
                    }
                };
                let mut results = Vec::with_capacity(batch.len());
                for request in batch {
                    let result = (|| {
                        let child = state
                            .environment
                            .nested_write_txn(&mut parent)
                            .map_err(database_error)?;
                        let user = token::authenticate_user_in_txn(&state, &child, &request.bearer)
                            .map_err(ActionError::Authentication)?;
                        create_document(
                            &state,
                            &user,
                            request.input,
                            &request.id,
                            request.retry,
                            child,
                        )
                    })();
                    results.push((request.reply, result));
                }
                let commit = parent.commit();
                for (reply, result) in results {
                    let result = match (result, &commit) {
                        (Ok(document), Ok(())) => Ok(document.publish(&PUBSUB_POOL)),
                        (Ok(_), Err(error)) => Err(ActionError::Authentication(
                            crate::storage::database_error_ref(error),
                        )),
                        (Err(error), _) => Err(error),
                    };
                    let _ = reply.send(result);
                }
            }
        })?;
    WRITER
        .set(sender)
        .expect("document writer initialized once");
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            loop {
                let state = maintenance_state.clone();
                match tokio::task::spawn_blocking(move || prune_retries(&state)).await {
                    Ok(Ok(RETRY_CLEANUP_BATCH)) => tokio::task::yield_now().await,
                    Ok(Ok(_)) => break,
                    error => {
                        eprintln!("Idempotency cleanup failed: {error:?}");
                        break;
                    }
                }
            }
        }
    });
    Ok(())
}
