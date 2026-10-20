use super::*;

pub(super) fn read(context: &Context<'_>, query: &Query) -> Result<Data> {
    document_page(context, query, false, false)
}
