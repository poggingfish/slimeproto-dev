use super::{PubSub, SubscriberId};

use crate::{
    AppState,
    types::{Collection, Data, Value, valid_uuid},
};

#[derive(Default)]
pub(super) struct SubscriptionState {
    in_firehose: bool,
    has_subscribed_to_other_topic: bool,
}

enum Topic {
    Firehose,
    User(String),
    Collection(String),
}

impl Topic {
    fn parse(data: &Data) -> Result<Self, String> {
        let kind = match data.get("type") {
            Some(Value::String(kind)) => kind.as_str(),
            _ => return Err("type must be a string".into()),
        };
        if kind == "firehose" {
            return Ok(Self::Firehose);
        }
        if kind != "user" && kind != "collection" {
            return Err("expected type to be one of: collection, firehose, user".into());
        }
        let uuid = match data.get("uuid") {
            Some(Value::String(uuid)) if valid_uuid(uuid) => uuid.clone(),
            _ => return Err("invalid uuid".into()),
        };
        Ok(if kind == "user" {
            Self::User(uuid)
        } else {
            Self::Collection(uuid)
        })
    }

    fn name(&self) -> String {
        match self {
            Self::Firehose => "firehose".into(),
            Self::User(uuid) => format!("user/{uuid}"),
            Self::Collection(uuid) => format!("collection/{uuid}"),
        }
    }
}

fn internal(error: impl std::fmt::Display) -> String {
    eprintln!("Subscription lookup failed: {error}");
    "internal server error".into()
}

fn resolve(state: &AppState, topic: &Topic) -> Result<bool, String> {
    let (env, database) = (&state.environment, &state.database);
    let txn = env.read_txn().map_err(internal)?;
    match topic {
        Topic::User(uuid) => {
            let key = format!("user:{uuid}");
            if database
                .get(&txn, key.as_bytes())
                .map_err(internal)?
                .is_none()
            {
                return Err("user does not exist".into());
            }
            Ok(true)
        }
        Topic::Collection(uuid) => {
            let key = format!("collection:{uuid}");
            let encoded = database
                .get(&txn, key.as_bytes())
                .map_err(internal)?
                .ok_or("collection does not exist")?;
            let collection: Collection = serde_json::from_slice(encoded).map_err(internal)?;
            Ok(collection.listed)
        }
        Topic::Firehose => Ok(true),
    }
}

pub(super) async fn subscribe(
    state: AppState,
    pool: &PubSub,
    subscriber: SubscriberId,
    policy: &mut SubscriptionState,
    data: &Data,
) -> Result<(), String> {
    let topic = Topic::parse(data)?;
    let name = topic.name();
    if pool.is_subscribed(subscriber, &name) {
        return Ok(());
    }
    if matches!(topic, Topic::Firehose) {
        if policy.has_subscribed_to_other_topic {
            return Err("firehose must be the first subscription".into());
        }
        if !pool.subscribe(subscriber, &name) {
            return Err("subscription unavailable".into());
        }
        policy.in_firehose = true;
        return Ok(());
    }
    let listed = tokio::task::spawn_blocking(move || resolve(&state, &topic))
        .await
        .map_err(internal)??;
    if policy.in_firehose && listed {
        return Err("listed topics are already included in firehose".into());
    }
    if !pool.subscribe(subscriber, &name) {
        return Err("subscription unavailable".into());
    }
    policy.has_subscribed_to_other_topic = true;
    Ok(())
}

pub(super) fn unsubscribe(
    pool: &PubSub,
    subscriber: SubscriberId,
    policy: &mut SubscriptionState,
    data: &Data,
) -> Result<(), String> {
    let topic = Topic::parse(data)?;
    pool.unsubscribe(subscriber, &topic.name());
    if matches!(topic, Topic::Firehose) {
        policy.in_firehose = false;
    }
    Ok(())
}
