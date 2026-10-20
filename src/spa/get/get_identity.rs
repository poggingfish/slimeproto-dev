use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    fields(query, &["user"])?;
    let account_id = uuid(query, "user")?;
    let user: crate::types::UserRecord =
        context.record(&format!("user:{account_id}"), "user does not exist")?;
    let public = crate::identity::public_identity(&user.signing_seed);
    if public.account_id != account_id {
        return Err(internal("account key does not match its ID"));
    }
    Ok(data(&[
        ("account_id", &public.account_id),
        ("algorithm", public.algorithm),
        ("public_key", &public.public_key),
    ]))
}
