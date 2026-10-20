use crate::socket::PubSub;
use axum::{
    Json,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use sockudo_ws::Message;

use crate::{
    AppState, generate_uuid_v7,
    socket::PUBSUB_POOL,
    spa::{data, error_response},
    token::{self, AuthenticatedUser, TokenError},
    types::{Collection, Data, Event, Value},
};

#[derive(Debug)]
enum ActionError {
    Invalid(&'static str),
    Conflict,
    Authentication(TokenError),
    Internal(String),
}

impl ActionError {
    fn into_response(self, id: &str) -> Response {
        let (code, message) = match self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, message.to_owned()),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "collection name already exists".into(),
            ),
            Self::Authentication(TokenError::Unauthorized(message)) => {
                (StatusCode::UNAUTHORIZED, message.into())
            }
            Self::Authentication(TokenError::Request(code, message)) => (code, message),
            Self::Authentication(error @ TokenError::RateLimited(_)) => {
                return error.into_response();
            }
            Self::Internal(error) | Self::Authentication(TokenError::Internal(error)) => {
                eprintln!("Collection creation error: {error}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
        };
        error_response(code, "create_collection_err", id, &message)
    }
}

fn database_error(error: heed::Error) -> ActionError {
    ActionError::Authentication(crate::storage::database_error(error))
}

fn internal(error: impl std::fmt::Display) -> ActionError {
    ActionError::Internal(error.to_string())
}

struct CreateCollection {
    name: String,
    listed: bool,
}

impl CreateCollection {
    fn parse(data: &Data) -> Result<Self, ActionError> {
        let name = match data.get("name") {
            Some(Value::String(name)) if crate::names::valid_collection_name(name) => name.clone(),
            _ => {
                return Err(ActionError::Invalid(
                    "name must be a nonblank string of at most 256 bytes",
                ));
            }
        };
        let listed = match data.get("listed") {
            None => false,
            Some(Value::String(value)) if value == "true" => true,
            Some(Value::String(value)) if value == "false" => false,
            _ => {
                return Err(ActionError::Invalid(
                    "listed must be the string true or false",
                ));
            }
        };
        Ok(Self { name, listed })
    }
}

pub(crate) async fn handle(
    state: AppState,
    headers: HeaderMap,
    id: &str,
    input: Option<serde_json::Value>,
) -> Response {
    let input = input.and_then(|value| serde_json::from_value::<Data>(value).ok());
    let input = match input.as_ref().map(CreateCollection::parse) {
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
    let bearer = match bearer {
        Some(bearer) => bearer,
        None => {
            return ActionError::Authentication(TokenError::Unauthorized(
                "missing or invalid bearer token",
            ))
            .into_response(id);
        }
    };
    let request_id = id.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let user =
            token::authenticate_user(&state, &bearer).map_err(ActionError::Authentication)?;
        create_collection(&state, &user, input, &request_id, &PUBSUB_POOL)
    })
    .await;
    match result {
        Ok(Ok(event)) => (StatusCode::CREATED, Json(event)).into_response(),
        Ok(Err(error)) => error.into_response(id),
        Err(error) => internal(error).into_response(id),
    }
}

fn create_collection(
    state: &AppState,
    user: &AuthenticatedUser,
    input: CreateCollection,
    id: &str,
    pool: &PubSub,
) -> Result<Event, ActionError> {
    let mut collection = Collection {
        uuid: generate_uuid_v7(),
        name: input.name,
        author: user.user_id.clone(),
        listed: input.listed,
        signature: String::new(),
    };
    let collection_key = format!("collection:{}", collection.uuid);

    let name_key =
        crate::names::collection_key(&user.user_id, &collection.name).map_err(internal)?;
    let (env, database) = (&state.environment, &state.database);
    let mut txn = env.write_txn().map_err(database_error)?;
    collection.signature = crate::identity::sign_collection(state, &txn, &collection)
        .map_err(ActionError::Authentication)?;
    let encoded = serde_json::to_vec(&collection).map_err(internal)?;
    let received = Event {
        event: "create_collection_recv".into(),
        id: id.into(),
        data: data(&[
            ("author", &user.user_id),
            ("uuid", &collection.uuid),
            ("name", &collection.name),
            ("listed", if collection.listed { "true" } else { "false" }),
            ("signature", &collection.signature),
        ]),
    };
    let publication = serde_json::to_string(&received).map_err(internal)?;
    if let Some(uuid) = database.get(&txn, &name_key).map_err(database_error)? {
        let uuid = std::str::from_utf8(uuid).map_err(internal)?;
        let encoded = database
            .get(&txn, format!("collection:{uuid}").as_bytes())
            .map_err(database_error)?
            .ok_or_else(|| internal("collection name index references a missing collection"))?;
        let existing: Collection = serde_json::from_slice(encoded).map_err(internal)?;
        if existing.uuid != uuid
            || existing.author != user.user_id
            || existing.name != collection.name
        {
            return Err(internal("collection name index collision or corruption"));
        }
        return Err(ActionError::Conflict);
    }
    crate::storage::charge(state, &mut txn, &user.user_id, encoded.len())
        .map_err(ActionError::Authentication)?;
    database
        .put(&mut txn, collection_key.as_bytes(), &encoded)
        .map_err(database_error)?;
    database
        .put(&mut txn, &name_key, collection.uuid.as_bytes())
        .map_err(database_error)?;
    database
        .put(
            &mut txn,
            format!("all_collection:{}:{}", user.user_id, collection.uuid).as_bytes(),
            b"",
        )
        .map_err(database_error)?;
    if collection.listed {
        let listed_key = format!("listed_collection:{}:{}", user.user_id, collection.uuid);
        database
            .put(&mut txn, listed_key.as_bytes(), b"")
            .map_err(database_error)?;
    }
    txn.commit().map_err(database_error)?;

    if collection.listed {
        pool.publish(
            &format!("user/{}", user.user_id),
            Message::text(publication.clone()),
        );
        pool.publish("firehose", Message::text(publication));
    }
    Ok(Event {
        event: "create_collection_ok".into(),
        id: id.into(),
        data: data(&[("uuid", &collection.uuid)]),
    })
}
