use std::{collections::HashMap, ops::Bound};

use axum::{
    Json,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use heed::{Database, RoTxn, types::Bytes};
use serde::de::DeserializeOwned;

use super::{data, error_response};
use crate::{
    AppState,
    token::{self, AuthenticatedUser, TokenError},
    types::{Collection, Data, Document, Event, Value, valid_event_id, valid_uuid},
};

mod catch_up_documents;
mod get_collection;
mod get_collection_uuid;
mod get_document;
mod get_document_content;
mod get_document_refs;
mod get_documents;
mod get_identity;
mod get_last_document;
mod get_storage_usage;
mod list_all_collections;
mod list_collections;
mod list_document_referrers;
mod list_document_referrers_content;
mod list_documents;
mod list_documents_content;
mod list_documents_reverse;
mod list_user_documents;

type Query = HashMap<String, String>;
type Row<'a> = (&'a [u8], &'a [u8]);
type Rows<'a> = Vec<Row<'a>>;
type Result<T> = std::result::Result<T, ReadError>;

struct ReadError(StatusCode, String);

fn invalid(message: impl Into<String>) -> ReadError {
    ReadError(StatusCode::BAD_REQUEST, message.into())
}
fn missing(message: &str) -> ReadError {
    ReadError(StatusCode::NOT_FOUND, message.into())
}
fn internal(error: impl std::fmt::Display) -> ReadError {
    eprintln!("SPA read error: {error}");
    ReadError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal server error".into(),
    )
}

fn decode(input: &str) -> Result<String> {
    let mut output = Vec::new();
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'+' => output.push(b' '),
            b'%' => {
                let high = bytes.next().and_then(|b| (b as char).to_digit(16));
                let low = bytes.next().and_then(|b| (b as char).to_digit(16));
                match (high, low) {
                    (Some(high), Some(low)) => output.push((high * 16 + low) as u8),
                    _ => return Err(invalid("invalid query encoding")),
                }
            }
            _ => output.push(byte),
        }
    }
    String::from_utf8(output).map_err(|_| invalid("invalid query encoding"))
}

fn parse_query(raw: &str) -> Result<Query> {
    let mut query = Query::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(key)?;
        let value = decode(value)?;
        if query.insert(key, value).is_some() {
            return Err(invalid("duplicate query parameter"));
        }
    }
    Ok(query)
}

fn fields(query: &Query, allowed: &[&str]) -> Result<()> {
    if query
        .keys()
        .any(|key| key != "event" && key != "id" && !allowed.contains(&key.as_str()))
    {
        return Err(invalid("unknown query parameter"));
    }
    Ok(())
}

fn required<'a>(query: &'a Query, key: &str) -> Result<&'a str> {
    query
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| invalid(format!("{key} is required")))
}

fn uuid<'a>(query: &'a Query, key: &str) -> Result<&'a str> {
    let value = required(query, key)?;
    if !valid_uuid(value) {
        return Err(invalid(format!("invalid {key} uuid")));
    }
    Ok(value)
}

fn page_options(query: &Query, max: usize) -> Result<(usize, Option<&str>)> {
    let limit = match query.get("limit") {
        None => max,
        Some(value) if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => value
            .parse::<usize>()
            .map_err(|_| invalid("invalid limit"))?,
        _ => return Err(invalid("invalid limit")),
    };
    if !(1..=max).contains(&limit) {
        return Err(invalid(format!("limit must be between 1 and {max}")));
    }
    let after = query.get("after").map(String::as_str);
    if after.is_some_and(|value| !valid_uuid(value)) {
        return Err(invalid("invalid cursor"));
    }
    Ok((limit, after))
}

pub(super) async fn handle(state: AppState, headers: HeaderMap, raw: &str) -> Response {
    let query = match parse_query(raw) {
        Ok(query) => query,
        Err(error) => return error_response(error.0, "generic_err", "", &error.1),
    };
    let id = query.get("id").cloned().unwrap_or_default();
    let event = query.get("event").cloned().unwrap_or_default();
    if !valid_event_id(&id) {
        return cache_policy(
            error_response(
                StatusCode::BAD_REQUEST,
                "generic_err",
                "",
                "id must contain 1-64 ASCII alphanumeric characters",
            ),
            &event,
        );
    }
    if !matches!(
        event.as_str(),
        "list_documents"
            | "get_last_document"
            | "list_documents_reverse"
            | "list_documents_content"
            | "get_document_refs"
            | "list_document_referrers"
            | "list_document_referrers_content"
            | "get_document_content"
            | "get_document"
            | "get_documents"
            | "list_collections"
            | "list_all_collections"
            | "get_identity"
            | "get_storage_usage"
            | "get_collection_uuid"
            | "get_collection"
            | "list_user_documents"
            | "catch_up_documents"
    ) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "generic_err",
            &id,
            "unknown or invalid event",
        );
    }
    let action = event.clone();
    let result =
        tokio::task::spawn_blocking(move || execute(&state, &headers, &action, &query)).await;
    let response = match result {
        Ok(Ok(data)) => Json(Event {
            event: format!("{event}_ok"),
            id,
            data,
        })
        .into_response(),
        Ok(Err(error)) => error_response(error.0, &format!("{event}_err"), &id, &error.1),
        Err(error) => {
            let error = internal(error);
            error_response(error.0, &format!("{event}_err"), &id, &error.1)
        }
    };
    cache_policy(response, &event)
}

fn cache_policy(mut response: Response, event: &str) -> Response {
    if matches!(event, "list_all_collections" | "get_storage_usage") {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

fn execute(state: &AppState, headers: &HeaderMap, action: &str, query: &Query) -> Result<Data> {
    let user = if matches!(action, "list_all_collections" | "get_storage_usage") {
        fields(
            query,
            if action == "get_storage_usage" {
                &["user"]
            } else {
                &["user", "after", "limit"]
            },
        )?;
        let requested = uuid(query, "user")?;
        page_options(query, 5000)?;
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, value)| scheme.eq_ignore_ascii_case("bearer") && value.len() == 37)
            .map(|(_, value)| value)
            .ok_or_else(|| {
                ReadError(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token".into(),
                )
            })?;
        let user = token::authenticate_user(state, bearer).map_err(|error| match error {
            TokenError::Unauthorized(message) => {
                ReadError(StatusCode::UNAUTHORIZED, message.into())
            }
            TokenError::Request(status, message) => ReadError(status, message),
            TokenError::RateLimited(_) => ReadError(
                StatusCode::TOO_MANY_REQUESTS,
                "request rate limit exceeded".into(),
            ),
            TokenError::Internal(error) => internal(error),
        })?;
        if user.user_id != requested {
            return Err(ReadError(
                StatusCode::FORBIDDEN,
                "cannot access another user's private data".into(),
            ));
        }
        Some(user)
    } else {
        None
    };
    let (env, database) = (&state.environment, &state.database);
    let context = Context {
        database,
        txn: env.read_txn().map_err(internal)?,
        user,
        max_storage: state.config.max_storage_per_user_bytes,
    };
    match action {
        "list_documents" => list_documents::read(&context, query),
        "get_last_document" => get_last_document::read(&context, query),
        "list_documents_reverse" => list_documents_reverse::read(&context, query),
        "list_documents_content" => list_documents_content::read(&context, query),
        "get_document_refs" => get_document_refs::read(&context, query),
        "list_document_referrers" => list_document_referrers::read(&context, query),
        "list_document_referrers_content" => list_document_referrers_content::read(&context, query),
        "get_document_content" => get_document_content::read(&context, query),
        "get_document" => get_document::read(&context, query),
        "get_documents" => get_documents::read(&context, query),
        "list_collections" => list_collections::read(&context, query),
        "list_all_collections" => list_all_collections::read(&context, query),
        "get_identity" => get_identity::read(&context, query),
        "get_storage_usage" => get_storage_usage::read(&context, query),
        "get_collection_uuid" => get_collection_uuid::read(&context, query),
        "get_collection" => get_collection::read(&context, query),
        "list_user_documents" => list_user_documents::read(&context, query),
        "catch_up_documents" => catch_up_documents::read(&context, query),
        _ => unreachable!(),
    }
}

struct Context<'a> {
    database: &'a Database<Bytes, Bytes>,
    txn: RoTxn<'a, heed::WithTls>,
    user: Option<AuthenticatedUser>,
    max_storage: Option<u64>,
}

impl Context<'_> {
    fn public_document(
        &self,
        uuid: &str,
        seq: u64,
        user: Option<&str>,
    ) -> Result<(Document, Collection)> {
        let document = self.document(uuid).map_err(|error| internal(error.1))?;
        let collection = self
            .collection(&document.collection)
            .map_err(|error| internal(error.1))?;
        if document.seq != seq || !collection.listed {
            return Err(internal("inconsistent public document index"));
        }
        if let Some(user) = user
            && collection.author != user
        {
            return Err(internal("inconsistent user document index"));
        }
        Ok((document, collection))
    }
    fn indexed_uuid(&self, value: &[u8], seq: u64) -> Result<String> {
        let uuid = std::str::from_utf8(value).map_err(internal)?;
        if !valid_uuid(uuid)
            || self.get(&sequence_key(b"document_seq:", seq))? != Some(value)
            || self.get(format!("document:{uuid}").as_bytes())?.is_none()
        {
            return Err(internal("inconsistent document index"));
        }
        Ok(uuid.into())
    }
    fn get(&self, key: &[u8]) -> Result<Option<&[u8]>> {
        self.database.get(&self.txn, key).map_err(internal)
    }
    fn record<T: DeserializeOwned>(&self, key: &str, message: &str) -> Result<T> {
        serde_json::from_slice(self.get(key.as_bytes())?.ok_or_else(|| missing(message))?)
            .map_err(internal)
    }
    fn collection(&self, uuid: &str) -> Result<Collection> {
        let collection: Collection =
            self.record(&format!("collection:{uuid}"), "collection does not exist")?;
        if collection.uuid != uuid {
            return Err(internal("inconsistent collection record"));
        }
        Ok(collection)
    }
    fn document(&self, uuid: &str) -> Result<Document> {
        let document: Document =
            self.record(&format!("document:{uuid}"), "document does not exist")?;
        if document.uuid != uuid {
            return Err(internal("inconsistent document record"));
        }
        Ok(document)
    }
    fn user_exists(&self, uuid: &str) -> Result<()> {
        if self.get(format!("user:{uuid}").as_bytes())?.is_none() {
            return Err(missing("user does not exist"));
        }
        Ok(())
    }
    fn scan(
        &self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        reverse: bool,
        count: usize,
    ) -> Result<Rows<'_>> {
        let range = (lower, upper);
        let iter: Box<dyn Iterator<Item = heed::Result<Row<'_>>> + '_> = if reverse {
            Box::new(
                self.database
                    .rev_range(&self.txn, &range)
                    .map_err(internal)?,
            )
        } else {
            Box::new(self.database.range(&self.txn, &range).map_err(internal)?)
        };
        iter.take(count)
            .map(|entry| entry.map_err(internal))
            .collect()
    }
}

fn sequence_key(prefix: &[u8], seq: u64) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.extend(seq.to_be_bytes());
    key
}

fn index_sequence(key: &[u8], prefix: &[u8]) -> Result<u64> {
    if !key.starts_with(prefix) || key.len() != prefix.len() + 8 {
        return Err(internal("invalid document sequence index"));
    }
    Ok(u64::from_be_bytes(
        key[prefix.len()..].try_into().map_err(internal)?,
    ))
}

fn document_data(document: Document) -> Data {
    let mut fields = data(&[
        ("uuid", &document.uuid),
        ("seq", &document.seq.to_string()),
        ("collection", &document.collection),
        ("type", &document.r#type),
        ("author", &document.author),
        ("signature", &document.signature),
    ]);
    if let Some(name) = document.name {
        fields.insert("name".into(), Value::String(name));
    }
    fields.insert(
        "refs".into(),
        Value::Object(crate::types::refs_data(&document.refs)),
    );
    fields.insert("content".into(), Value::Object(document.content));
    fields
}

fn collection_data(collection: Collection) -> Data {
    data(&[
        ("uuid", &collection.uuid),
        ("name", &collection.name),
        ("author", &collection.author),
        ("signature", &collection.signature),
        ("listed", if collection.listed { "true" } else { "false" }),
    ])
}

fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    let last = end.last_mut().expect("nonempty ASCII index prefix");
    *last += 1;
    end
}

fn page(field: &str, entries: Vec<Value>, last_uuid: &str, has_more: bool) -> Data {
    let mut result = data(&[
        ("next", if has_more { last_uuid } else { "" }),
        ("has_more", if has_more { "true" } else { "false" }),
    ]);
    result.insert(
        field.into(),
        Value::Object(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value))
                .collect(),
        ),
    );
    result
}

fn document_page(
    context: &Context<'_>,
    query: &Query,
    reverse: bool,
    content: bool,
) -> Result<Data> {
    fields(query, &["collection", "after", "limit"])?;
    let collection = uuid(query, "collection")?;
    let (limit, after) = page_options(query, if content { 500 } else { 5000 })?;
    let (rows, has_more) = document_rows(context, collection, after, limit, reverse)?;
    let prefix = format!("collection_document:{collection}:");
    let mut entries = Vec::with_capacity(rows.len());
    let mut last = String::new();
    for (key, value) in rows {
        let seq = index_sequence(key, prefix.as_bytes())?;
        if content {
            last = std::str::from_utf8(value).map_err(internal)?.into();
            let document = context.document(&last).map_err(|error| internal(error.1))?;
            if document.collection != collection || document.seq != seq {
                return Err(internal("inconsistent collection document index"));
            }
            entries.push(Value::Object(document_data(document)));
        } else {
            last = context.indexed_uuid(value, seq)?;
            entries.push(Value::String(last.clone()));
        }
    }
    Ok(page("documents", entries, &last, has_more))
}

fn document_rows<'a>(
    context: &'a Context<'_>,
    collection: &str,
    after: Option<&str>,
    limit: usize,
    reverse: bool,
) -> Result<(Rows<'a>, bool)> {
    context.collection(collection)?;
    let prefix = format!("collection_document:{collection}:").into_bytes();
    let end = prefix_end(&prefix);
    let cursor = if let Some(uuid) = after {
        let document = context.document(uuid).map_err(|error| {
            if error.0 == StatusCode::NOT_FOUND {
                invalid("invalid cursor")
            } else {
                error
            }
        })?;
        if document.collection != collection {
            return Err(invalid("invalid cursor"));
        }
        let mut key = prefix.clone();
        key.extend(document.seq.to_be_bytes());
        if context.get(&key)? != Some(uuid.as_bytes()) {
            return Err(internal("inconsistent document cursor index"));
        }
        Some(key)
    } else {
        None
    };
    let lower = if !reverse && cursor.is_some() {
        Bound::Excluded(cursor.as_deref().unwrap())
    } else {
        Bound::Included(prefix.as_slice())
    };
    let upper = if reverse && cursor.is_some() {
        Bound::Excluded(cursor.as_deref().unwrap())
    } else {
        Bound::Excluded(end.as_slice())
    };
    let mut rows = context.scan(lower, upper, reverse, limit + 1)?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    Ok((rows, has_more))
}

fn documents(
    context: &Context<'_>,
    collection: &str,
    after: Option<&str>,
    limit: usize,
    reverse: bool,
) -> Result<(Vec<Document>, bool)> {
    let (rows, has_more) = document_rows(context, collection, after, limit, reverse)?;
    let prefix = format!("collection_document:{collection}:");
    let mut documents = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let seq = index_sequence(key, prefix.as_bytes())?;
        let uuid = std::str::from_utf8(value).map_err(internal)?;
        let document = context.document(uuid).map_err(|error| internal(error.1))?;
        if document.collection != collection || document.seq != seq {
            return Err(internal("inconsistent collection document index"));
        }
        documents.push(document);
    }
    Ok((documents, has_more))
}

fn collection_page(context: &Context<'_>, query: &Query, all: bool) -> Result<Data> {
    fields(query, &["user", "after", "limit"])?;
    let user = uuid(query, "user")?;
    let (limit, after) = page_options(query, 5000)?;
    context.user_exists(user)?;
    if all
        && context
            .user
            .as_ref()
            .is_none_or(|principal| principal.user_id != user)
    {
        return Err(ReadError(
            StatusCode::FORBIDDEN,
            "cannot list another user's private collections".into(),
        ));
    }
    let prefix = format!(
        "{}:{user}:",
        if all {
            "all_collection"
        } else {
            "listed_collection"
        }
    );
    let end = prefix_end(prefix.as_bytes());
    let cursor = after.map(|uuid| format!("{prefix}{uuid}"));
    if let Some(cursor) = &cursor
        && context.get(cursor.as_bytes())?.is_none()
    {
        return Err(invalid("invalid cursor"));
    }
    let lower = match &cursor {
        Some(cursor) => Bound::Excluded(cursor.as_bytes()),
        None => Bound::Included(prefix.as_bytes()),
    };
    let rows = context.scan(lower, Bound::Excluded(&end), false, limit + 1)?;
    let has_more = rows.len() > limit;
    let mut entries = Vec::new();
    let mut last = String::new();
    for (key, _) in rows.into_iter().take(limit) {
        let uuid = std::str::from_utf8(&key[prefix.len()..]).map_err(internal)?;
        let collection = context
            .collection(uuid)
            .map_err(|error| internal(error.1))?;
        if !all && !collection.listed {
            return Err(internal("unlisted collection in public index"));
        }
        let owner_key = crate::names::collection_key(user, &collection.name).map_err(internal)?;
        if collection.author != user || context.get(&owner_key)? != Some(uuid.as_bytes()) {
            return Err(internal("inconsistent collection owner index"));
        }
        entries.push(Value::Object(collection_data(collection)));
        last = uuid.to_owned();
    }
    Ok(page("collections", entries, &last, has_more))
}

fn referrer_page(context: &Context<'_>, query: &Query, content: bool) -> Result<Data> {
    fields(query, &["document", "after", "limit"])?;
    let target = uuid(query, "document")?;
    context.document(target)?;
    let (limit, after) = page_options(query, if content { 500 } else { 5000 })?;
    let prefix = format!("document_referrer:{target}:").into_bytes();
    let end = prefix_end(&prefix);
    let cursor = if let Some(uuid) = after {
        let source = context.document(uuid).map_err(|error| {
            if error.0 == StatusCode::NOT_FOUND {
                invalid("invalid cursor")
            } else {
                error
            }
        })?;
        let key = sequence_key(&prefix, source.seq);
        if !source.refs.iter().any(|reference| reference == target)
            || context.get(&key)? != Some(uuid.as_bytes())
            || !context.collection(&source.collection)?.listed
        {
            return Err(invalid("invalid cursor"));
        }
        Some(key)
    } else {
        None
    };
    let lower = match &cursor {
        Some(key) => Bound::Excluded(key.as_slice()),
        None => Bound::Included(prefix.as_slice()),
    };
    let rows = context.scan(lower, Bound::Excluded(&end), false, limit + 1)?;
    let has_more = rows.len() > limit;
    let mut entries = Vec::new();
    let mut last = String::new();
    for (key, value) in rows.into_iter().take(limit) {
        let seq = index_sequence(key, &prefix)?;
        last = context.indexed_uuid(value, seq)?;
        if context.get(&sequence_key(b"public_document_seq:", seq))? != Some(value) {
            return Err(internal("unlisted document in referrer index"));
        }
        if content {
            let (source, _) = context.public_document(&last, seq, None)?;
            if !source.refs.iter().any(|reference| reference == target) {
                return Err(internal("inconsistent document referrer index"));
            }
            entries.push(Value::Object(document_data(source)));
        } else {
            entries.push(Value::String(last.clone()));
        }
    }
    Ok(page("documents", entries, &last, has_more))
}
