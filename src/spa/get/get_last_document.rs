use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    fields(query, &["collection"])?;
    let collection = uuid(query, "collection")?;
    let (documents, _) = documents(context, collection, None, 1, true)?;
    Ok(data(&[(
        "uuid",
        documents
            .first()
            .map(|document| document.uuid.as_str())
            .unwrap_or(""),
    )]))
}
