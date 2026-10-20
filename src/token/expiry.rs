use std::ops::Bound;

use heed::{Database, Env, types::Bytes};

use super::{Session, TokenError, internal, now};

const PREFIX: &[u8] = b"token_expiry:";
const MARKER: &[u8] = b"migration:token_expiry:v1";
const BATCH: usize = 256;

pub(super) fn key(expires_at: u64, token_key: &[u8]) -> Vec<u8> {
    let mut key = PREFIX.to_vec();
    key.extend_from_slice(&expires_at.to_be_bytes());
    key.extend_from_slice(token_key);
    key
}

pub(crate) fn initialize_sessions(
    env: &Env,
    database: Database<Bytes, Bytes>,
) -> Result<(), TokenError> {
    let needs_migration = {
        let txn = env.read_txn().map_err(crate::storage::database_error)?;
        database
            .get(&txn, MARKER)
            .map_err(crate::storage::database_error)?
            .is_none()
    };
    if needs_migration {
        let mut cursor = b"token:".to_vec();
        loop {
            let mut txn = env.write_txn().map_err(crate::storage::database_error)?;
            let range = (
                Bound::Excluded(cursor.as_slice()),
                Bound::Excluded(b"token;".as_slice()),
            );
            let rows = database
                .range(&txn, &range)
                .map_err(crate::storage::database_error)?
                .take(BATCH)
                .map(|row| {
                    let (key, value) = row.map_err(internal)?;
                    let session: Session = serde_json::from_slice(value).map_err(internal)?;
                    Ok((key.to_vec(), session.expires_at))
                })
                .collect::<Result<Vec<_>, TokenError>>()?;
            let timestamp = now()?;
            for (token_key, expires_at) in &rows {
                if *expires_at <= timestamp {
                    database
                        .delete(&mut txn, token_key)
                        .map_err(crate::storage::database_error)?;
                } else {
                    database
                        .put(&mut txn, &key(*expires_at, token_key), b"")
                        .map_err(crate::storage::database_error)?;
                }
            }
            let done = rows.len() < BATCH;
            if done {
                database
                    .put(&mut txn, MARKER, b"complete")
                    .map_err(crate::storage::database_error)?;
            }
            txn.commit().map_err(crate::storage::database_error)?;
            if done {
                break;
            }
            cursor = rows.last().expect("full migration batch").0.clone();
        }
    }
    while prune_sessions(env, database)? == BATCH {}
    Ok(())
}

pub(crate) fn prune_sessions(
    env: &Env,
    database: Database<Bytes, Bytes>,
) -> Result<usize, TokenError> {
    let timestamp = now()?;
    let mut txn = env.write_txn().map_err(crate::storage::database_error)?;
    let expired = database
        .prefix_iter(&txn, PREFIX)
        .map_err(crate::storage::database_error)?
        .take(BATCH)
        .map(|row| {
            let (key, _) = row.map_err(internal)?;
            let bytes = key
                .get(PREFIX.len()..PREFIX.len() + 8)
                .ok_or_else(|| internal("invalid session expiry index"))?;
            let expires_at = u64::from_be_bytes(bytes.try_into().map_err(internal)?);
            let token_key = &key[PREFIX.len() + 8..];
            if token_key.len() != b"token:".len() + 32 || !token_key.starts_with(b"token:") {
                return Err(internal("invalid session expiry token key"));
            }
            Ok((key.to_vec(), expires_at))
        })
        .take_while(|row| !matches!(row, Ok((_, expiry)) if *expiry > timestamp))
        .collect::<Result<Vec<_>, TokenError>>()?;
    if expired.is_empty() {
        return Ok(0);
    }
    let mut removed = Vec::new();
    for (expiry, _) in &expired {
        let token_key = &expiry[PREFIX.len() + 8..];
        if let Some(value) = database
            .get(&txn, token_key)
            .map_err(crate::storage::database_error)?
        {
            let session: Session = serde_json::from_slice(value).map_err(internal)?;
            if session.expires_at <= timestamp {
                database
                    .delete(&mut txn, token_key)
                    .map_err(crate::storage::database_error)?;
                removed.push(token_key.to_vec());
            } else {
                database
                    .put(&mut txn, &key(session.expires_at, token_key), b"")
                    .map_err(crate::storage::database_error)?;
            }
        }
        database
            .delete(&mut txn, expiry)
            .map_err(crate::storage::database_error)?;
    }
    txn.commit().map_err(crate::storage::database_error)?;
    for token_key in removed {
        crate::socket::revoke_session(&token_key);
    }
    Ok(expired.len())
}

pub(crate) fn start_cleanup(state: crate::AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            loop {
                let state = state.clone();
                match tokio::task::spawn_blocking(move || {
                    prune_sessions(&state.environment, state.database)
                })
                .await
                {
                    Ok(Ok(BATCH)) => tokio::task::yield_now().await,
                    Ok(Ok(_)) => break,
                    error => {
                        eprintln!("Session cleanup failed: {error:?}");
                        break;
                    }
                }
            }
        }
    });
}
