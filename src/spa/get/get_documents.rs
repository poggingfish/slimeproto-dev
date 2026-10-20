use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    fields(query, &["documents"])?;
    let input = required(query, "documents")?;
    let mut uuids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for uuid in input.split(',') {
        if uuids.len() == 500 {
            return Err(invalid("documents must contain at most 500 UUIDs"));
        }
        if !valid_uuid(uuid) {
            return Err(invalid("invalid document uuid"));
        }
        if !seen.insert(uuid) {
            return Err(invalid("duplicate document uuid"));
        }
        uuids.push(uuid);
    }
    let documents = uuids
        .into_iter()
        .enumerate()
        .map(|(index, uuid)| {
            let document = context.document(uuid)?;
            Ok((index.to_string(), Value::Object(document_data(document))))
        })
        .collect::<Result<Data>>()?;
    Ok(Data::from([("documents".into(), Value::Object(documents))]))
}
