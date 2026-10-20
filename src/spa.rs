use axum::{
    Json,
    extract::{RawQuery, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use crate::{
    AppState,
    types::{Data, Event, Value, valid_event_id},
};

mod get;
mod migration;
mod post;
pub(crate) use migration::{
    migrate_collection_index, migrate_document_indexes, migrate_reference_index,
};

pub(crate) async fn spa_get_handler(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    get::handle(state, headers, query.as_deref().unwrap_or("")).await
}

fn data(entries: &[(&str, &str)]) -> Data {
    entries
        .iter()
        .map(|(key, value)| (key.to_string(), Value::String(value.to_string())))
        .collect()
}

fn error_response(code: StatusCode, event: &str, id: &str, message: &str) -> Response {
    (
        code,
        Json(Event {
            event: event.into(),
            id: id.into(),
            data: data(&[("fakywaky", &message.to_lowercase())]),
        }),
    )
        .into_response()
}

pub(crate) async fn spa_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(error) => return error_response(error.status(), "generic_err", "", &error.body_text()),
    };
    let id = request
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !valid_event_id(id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "generic_err",
            "",
            "id must contain 1-64 ASCII alphanumeric characters",
        );
    }
    match request.get("event").and_then(serde_json::Value::as_str) {
        Some("create_collection") => {
            post::create_collection::handle(state, headers, id, request.get("data").cloned()).await
        }
        Some("create_document") => {
            post::create_document::handle(state, headers, id, request.get("data").cloned()).await
        }
        Some("send") => post::send::handle(state, headers, id, request.get("data").cloned()).await,
        Some("revoke_token") => {
            post::revoke_token::handle(state, headers, id, request.get("data").cloned()).await
        }
        _ => error_response(
            StatusCode::BAD_REQUEST,
            "generic_err",
            id,
            "unknown or invalid event",
        ),
    }
}

pub(crate) use post::create_document::start_writer as start_document_writer;
