use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    let user = uuid(query, "user")?;
    let used = crate::storage::usage(*context.database, &context.txn, user)
        .map_err(|error| internal(format!("storage usage: {error:?}")))?;
    Ok(data(&[
        ("used_bytes", &used.to_string()),
        (
            "max_bytes",
            &context
                .max_storage
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
    ]))
}
