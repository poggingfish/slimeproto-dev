use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    String(String),
    Object(ObjectData),
}

pub type ObjectData = HashMap<String, Value>;
pub type Data = ObjectData;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub id: String,
    pub data: Data,
}

pub(crate) fn valid_event_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

pub(crate) fn valid_uuid(uuid: &str) -> bool {
    let groups: Vec<_> = uuid.split(':').collect();
    groups
        .iter()
        .map(|group| group.len())
        .eq([12, 4, 4, 4, 4, 4])
        && groups.iter().all(|group| {
            group
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

#[derive(Deserialize, Serialize)]
pub(crate) struct UserRecord {
    pub password_hash: String,
    pub signing_seed: [u8; 32],
}

impl Drop for UserRecord {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.signing_seed.zeroize();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub uuid: String,
    pub name: String,
    pub author: String,
    pub listed: bool,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub seq: u64,
    pub uuid: String,
    pub collection: String,
    pub author: String,
    pub signature: String,
    pub r#type: String,
    pub name: Option<String>,
    pub content: Data,
    #[serde(default)]
    pub refs: Vec<String>,
}

pub(crate) const MAX_DOCUMENT_REFS: usize = 64;

pub(crate) fn refs_data(refs: &[String]) -> Data {
    refs.iter()
        .enumerate()
        .map(|(index, uuid)| (index.to_string(), Value::String(uuid.clone())))
        .collect()
}
