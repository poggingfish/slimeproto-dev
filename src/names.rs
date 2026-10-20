use sha2::{Digest, Sha256};

pub(crate) const MAX_COLLECTION_NAME_BYTES: usize = 256;
pub(crate) const MAX_DOCUMENT_NAME_BYTES: usize = 128;
const LEGACY_KEY_LIMIT: usize = 511;

pub(crate) fn valid_collection_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= MAX_COLLECTION_NAME_BYTES
}

fn key(kind: &str, scope: &str, name: &str) -> Result<Vec<u8>, serde_json::Error> {
    let encoded = serde_json::to_vec(&(scope, name))?;
    let mut key = format!("{kind}_name:").into_bytes();
    if key.len() + encoded.len() <= LEGACY_KEY_LIMIT {
        key.extend_from_slice(&encoded);
    } else {
        key = format!("{kind}_name_hash:").into_bytes();
        key.extend_from_slice(&Sha256::digest(&encoded));
    }
    Ok(key)
}

pub(crate) fn collection_key(owner: &str, name: &str) -> Result<Vec<u8>, serde_json::Error> {
    key("collection", owner, name)
}

pub(crate) fn document_key(collection: &str, name: &str) -> Result<Vec<u8>, serde_json::Error> {
    key("document", collection, name)
}
