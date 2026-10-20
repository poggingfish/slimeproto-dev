use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    referrer_page(context, query, true)
}
