use std::collections::HashMap;

use axum::http::StatusCode;
use heed::{Database, Env, RoTxn, RwTxn, types::Bytes};

use crate::{
    AppState, identity,
    token::TokenError,
    types::{Collection, Document},
};

pub(crate) fn database_error(error: heed::Error) -> TokenError {
    database_error_ref(&error)
}

pub(crate) fn database_error_ref(error: &heed::Error) -> TokenError {
    match error {
        heed::Error::Mdb(heed::MdbError::MapFull) => TokenError::Request(
            StatusCode::INSUFFICIENT_STORAGE,
            "database capacity exceeded; increase database_map_size_bytes and restart".into(),
        ),
        heed::Error::Io(error) if error.kind() == std::io::ErrorKind::StorageFull => {
            TokenError::Request(
                StatusCode::INSUFFICIENT_STORAGE,
                "database storage is full; free disk space and retry".into(),
            )
        }
        error => identity::internal(error),
    }
}

pub(crate) fn usage(
    database: Database<Bytes, Bytes>,
    txn: &RoTxn<'_>,
    user: &str,
) -> Result<u64, TokenError> {
    match database
        .get(txn, format!("storage_usage:{user}").as_bytes())
        .map_err(crate::storage::database_error)?
    {
        Some(bytes) => Ok(u64::from_be_bytes(
            bytes.try_into().map_err(identity::internal)?,
        )),
        None => Ok(0),
    }
}

pub(crate) fn charge(
    state: &AppState,
    txn: &mut RwTxn<'_>,
    user: &str,
    bytes: usize,
) -> Result<(), TokenError> {
    let used = usage(state.database, txn, user)?;
    let total = used
        .checked_add(u64::try_from(bytes).map_err(identity::internal)?)
        .ok_or_else(|| identity::internal("storage usage overflow"))?;
    if state
        .config
        .max_storage_per_user_bytes
        .is_some_and(|limit| total > limit)
    {
        return Err(TokenError::Request(
            StatusCode::INSUFFICIENT_STORAGE,
            "account storage limit exceeded".into(),
        ));
    }
    state
        .database
        .put(
            txn,
            format!("storage_usage:{user}").as_bytes(),
            &total.to_be_bytes(),
        )
        .map_err(crate::storage::database_error)?;
    Ok(())
}

pub(crate) fn migrate(
    env: &Env,
    database: Database<Bytes, Bytes>,
) -> Result<(), Box<dyn std::error::Error>> {
    const MARKER: &[u8] = b"migration:storage_usage:v1";
    let mut txn = env.write_txn()?;
    if database.get(&txn, MARKER)?.is_some() {
        return Ok(());
    }
    let mut totals = HashMap::<String, u64>::new();
    for prefix in [b"collection:".as_slice(), b"document:"] {
        for row in database.prefix_iter(&txn, prefix)? {
            let (_, value) = row?;
            let author = if prefix == b"collection:" {
                serde_json::from_slice::<Collection>(value)?.author
            } else {
                serde_json::from_slice::<Document>(value)?.author
            };
            if database
                .get(&txn, format!("user:{author}").as_bytes())?
                .is_none()
            {
                return Err("storage record references a missing account".into());
            }
            let total = totals.entry(author).or_default();
            *total = total
                .checked_add(value.len().try_into()?)
                .ok_or("storage usage overflow")?;
        }
    }
    let existing = database
        .prefix_iter(&txn, b"storage_usage:")?
        .map(|row| row.map(|(key, _)| key.to_vec()))
        .collect::<Result<Vec<_>, _>>()?;
    for key in existing {
        database.delete(&mut txn, &key)?;
    }
    for (user, total) in totals {
        database.put(
            &mut txn,
            format!("storage_usage:{user}").as_bytes(),
            &total.to_be_bytes(),
        )?;
    }
    database.put(&mut txn, MARKER, b"complete")?;
    txn.commit()?;
    Ok(())
}
