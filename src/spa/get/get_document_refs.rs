use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    fields(query, &["document"])?;
    let document = context.document(uuid(query, "document")?)?;
    Ok(Data::from([(
        "refs".into(),
        Value::Object(crate::types::refs_data(&document.refs)),
    )]))
}
