use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    collection_page(context, query, false)
}
