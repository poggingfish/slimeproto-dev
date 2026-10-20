use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    fields(query, &["collection"])?;
    let collection = context.collection(uuid(query, "collection")?)?;
    Ok(collection_data(collection))
}
