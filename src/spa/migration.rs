use heed::{Database, Env, types::Bytes};

use crate::types::{Collection, Document, UserRecord, valid_uuid};

pub(crate) fn migrate_reference_index(
    env: &Env,
    database: Database<Bytes, Bytes>,
) -> Result<(), Box<dyn std::error::Error>> {
    const MARKER: &[u8] = b"migration:document_referrers:v1";
    let mut txn = env.write_txn()?;
    if database.get(&txn, MARKER)?.is_some() {
        return Ok(());
    }
    let mut indexes = Vec::new();
    for entry in database.prefix_iter(&txn, b"document_seq:")? {
        let (key, value) = entry?;
        let seq = u64::from_be_bytes(key[b"document_seq:".len()..].try_into()?);
        let uuid = std::str::from_utf8(value)?;
        let document: Document = serde_json::from_slice(
            database
                .get(&txn, format!("document:{uuid}").as_bytes())?
                .ok_or("reference index source document is missing")?,
        )?;
        if !valid_uuid(uuid) || document.uuid != uuid || document.seq != seq || seq == 0 {
            return Err("inconsistent reference source sequence index".into());
        }
        if document.refs.is_empty() {
            continue;
        }
        let collection: Collection = serde_json::from_slice(
            database
                .get(
                    &txn,
                    format!("collection:{}", document.collection).as_bytes(),
                )?
                .ok_or("reference source collection is missing")?,
        )?;
        if collection.uuid != document.collection
            || document.refs.len() > crate::types::MAX_DOCUMENT_REFS
        {
            return Err("invalid reference source document".into());
        }
        let mut seen = std::collections::HashSet::new();
        for target in &document.refs {
            if !valid_uuid(target) || !seen.insert(target) {
                return Err("invalid or duplicate document reference".into());
            }
            if database
                .get(&txn, format!("document:{target}").as_bytes())?
                .is_none()
            {
                return Err("referenced document is missing".into());
            }
            if collection.listed {
                let mut index = format!("document_referrer:{target}:").into_bytes();
                index.extend(seq.to_be_bytes());
                indexes.push((index, uuid.as_bytes().to_vec()));
            }
        }
    }
    for (key, value) in indexes {
        database.put(&mut txn, &key, &value)?;
    }
    database.put(&mut txn, MARKER, b"complete")?;
    txn.commit()?;
    Ok(())
}

pub(crate) fn migrate_document_indexes(
    env: &Env,
    database: Database<Bytes, Bytes>,
) -> Result<(), Box<dyn std::error::Error>> {
    const MARKER: &[u8] = b"migration:public_document_seq:v1";
    let mut txn = env.write_txn()?;
    if database.get(&txn, MARKER)?.is_some() {
        return Ok(());
    }
    let mut indexes = Vec::new();
    for entry in database.prefix_iter(&txn, b"document_seq:")? {
        let (key, value) = entry?;
        let seq = u64::from_be_bytes(key[b"document_seq:".len()..].try_into()?);
        let uuid = std::str::from_utf8(value)?;
        if !valid_uuid(uuid) {
            return Err("invalid document UUID in sequence index".into());
        }
        let document: Document = serde_json::from_slice(
            database
                .get(&txn, format!("document:{uuid}").as_bytes())?
                .ok_or("document sequence index references a missing document")?,
        )?;
        let collection: Collection = serde_json::from_slice(
            database
                .get(
                    &txn,
                    format!("collection:{}", document.collection).as_bytes(),
                )?
                .ok_or("document references a missing collection")?,
        )?;
        if document.uuid != uuid
            || document.seq != seq
            || document.collection != collection.uuid
            || seq == 0
        {
            return Err("inconsistent document sequence index".into());
        }
        if collection.listed {
            let user_id = &collection.author;
            let owner: UserRecord = serde_json::from_slice(
                database
                    .get(&txn, format!("user:{user_id}").as_bytes())?
                    .ok_or("listed document references a missing account")?,
            )?;
            if !valid_uuid(user_id)
                || crate::identity::public_identity(&owner.signing_seed).account_id != *user_id
            {
                return Err("inconsistent document owner".into());
            }
            for prefix in [
                b"public_document_seq:".to_vec(),
                format!("listed_document:{user_id}:").into_bytes(),
            ] {
                let mut index = prefix;
                index.extend(seq.to_be_bytes());
                indexes.push((index, uuid.as_bytes().to_vec()));
            }
        }
    }
    for (key, value) in indexes {
        database.put(&mut txn, &key, &value)?;
    }
    database.put(&mut txn, MARKER, b"complete")?;
    txn.commit()?;
    Ok(())
}

pub(crate) fn migrate_collection_index(
    env: &Env,
    database: Database<Bytes, Bytes>,
) -> Result<(), Box<dyn std::error::Error>> {
    const MARKER: &[u8] = b"migration:all_collection:v1";
    let mut txn = env.write_txn()?;
    if database.get(&txn, MARKER)?.is_some() {
        return Ok(());
    }
    let mut keys = Vec::new();
    for entry in database.prefix_iter(&txn, b"collection:")? {
        let (key, value) = entry?;
        let collection: Collection = serde_json::from_slice(value)?;
        let user_id = &collection.author;
        let uuid = &collection.uuid;
        if !valid_uuid(user_id)
            || !valid_uuid(uuid)
            || key != format!("collection:{uuid}").as_bytes()
        {
            return Err("invalid collection record".into());
        }
        let name_key = crate::names::collection_key(user_id, &collection.name)?;
        if database.get(&txn, &name_key)? != Some(uuid.as_bytes()) {
            return Err("inconsistent collection name index".into());
        }
        let user: UserRecord = serde_json::from_slice(
            database
                .get(&txn, format!("user:{user_id}").as_bytes())?
                .ok_or("collection index references a missing owner")?,
        )?;
        if crate::identity::public_identity(&user.signing_seed).account_id != *user_id {
            return Err("inconsistent collection owner".into());
        }
        keys.push(format!("all_collection:{user_id}:{uuid}"));
    }
    for key in keys {
        database.put(&mut txn, key.as_bytes(), b"")?;
    }
    database.put(&mut txn, MARKER, b"complete")?;
    txn.commit()?;
    Ok(())
}
