use super::*;

fn sequence(query: &Query, field: &str) -> Result<Option<u64>> {
    query
        .get(field)
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid(format!("invalid {field}")));
            }
            value
                .parse::<u64>()
                .map_err(|_| invalid(format!("invalid {field}")))
        })
        .transpose()
}

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    fields(query, &["user", "after_seq", "upto_seq", "limit"])?;
    let user = if query.contains_key("user") {
        let user = uuid(query, "user")?;
        context.user_exists(user)?;
        Some(user)
    } else {
        None
    };
    let (limit, _) = page_options(query, 500)?;
    let after = sequence(query, "after_seq")?.unwrap_or(0);
    let public_prefix = b"public_document_seq:";
    let public_end = prefix_end(public_prefix);
    let tail = context.scan(
        Bound::Included(public_prefix.as_slice()),
        Bound::Excluded(&public_end),
        true,
        1,
    )?;
    let public_seq = tail
        .first()
        .map(|(key, _)| index_sequence(key, public_prefix))
        .transpose()?
        .unwrap_or(0);
    let snapshot = sequence(query, "upto_seq")?.unwrap_or(public_seq.max(after));
    if snapshot < after || (snapshot > public_seq && snapshot != after) {
        return Err(invalid("invalid snapshot boundary"));
    }
    let prefix = match user {
        Some(user) => format!("listed_document:{user}:").into_bytes(),
        None => public_prefix.to_vec(),
    };
    let lower = sequence_key(&prefix, after);
    let upper = sequence_key(&prefix, snapshot);
    let rows = if after == snapshot {
        Vec::new()
    } else {
        context.scan(
            Bound::Excluded(&lower),
            Bound::Included(&upper),
            false,
            limit + 1,
        )?
    };
    let has_more = rows.len() > limit;
    let mut entries = Data::new();
    let mut last_seq = after;
    for (index, (key, value)) in rows.into_iter().take(limit).enumerate() {
        let seq = index_sequence(key, &prefix)?;
        let uuid = std::str::from_utf8(value).map_err(internal)?;
        let (document, collection) = context.public_document(uuid, seq, user)?;
        let mut fields = document_data(document);
        fields.insert("author".into(), Value::String(collection.author));
        fields.insert("collection_name".into(), Value::String(collection.name));
        entries.insert(index.to_string(), Value::Object(fields));
        last_seq = seq;
    }
    let mut result = data(&[
        (
            "next_seq",
            &if has_more { last_seq } else { snapshot }.to_string(),
        ),
        ("snapshot", &snapshot.to_string()),
        ("has_more", if has_more { "true" } else { "false" }),
    ]);
    result.insert("documents".into(), Value::Object(entries));
    Ok(result)
}
