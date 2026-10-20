use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    fields(query, &["user", "after", "limit"])?;
    let user = uuid(query, "user")?;
    context.user_exists(user)?;
    let (limit, after) = page_options(query, 5000)?;
    let prefix = format!("listed_document:{user}:").into_bytes();
    let end = prefix_end(&prefix);
    let cursor = if let Some(uuid) = after {
        let document = context.document(uuid).map_err(|error| {
            if error.0 == StatusCode::NOT_FOUND {
                invalid("invalid cursor")
            } else {
                error
            }
        })?;
        let key = sequence_key(&prefix, document.seq);
        if context.get(&key)? != Some(uuid.as_bytes()) {
            return Err(invalid("invalid cursor"));
        }
        context.public_document(uuid, document.seq, Some(user))?;
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
        entries.push(Value::String(last.clone()));
    }
    Ok(page("documents", entries, &last, has_more))
}
