use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    fields(query, &["user", "name"])?;
    let user = uuid(query, "user")?;
    let name = required(query, "name")?;
    if !crate::names::valid_collection_name(name) {
        return Err(invalid(
            "name must be a nonblank string of at most 256 bytes",
        ));
    }
    context.user_exists(user)?;
    let key = crate::names::collection_key(user, name).map_err(internal)?;
    let value = context
        .get(&key)?
        .ok_or_else(|| missing("collection does not exist"))?;
    let uuid = std::str::from_utf8(value).map_err(internal)?;
    let collection = context
        .collection(uuid)
        .map_err(|error| internal(error.1))?;
    if collection.name != name || collection.author != user {
        return Err(internal("inconsistent collection name index"));
    }
    if !collection.listed {
        return Err(missing("collection does not exist"));
    }
    Ok(data(&[("uuid", uuid)]))
}
