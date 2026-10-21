//! Post predicates use explicit extractor events, never file ID transitions.
use crate::{
    Result, event::ExtractionEvent, extract::Media, filter::FilterExpression,
    selection::RangeSelection,
};
use futures_util::stream::BoxStream;

#[derive(Clone, Debug)]
pub struct PostSelection {
    pub(crate) range: Option<RangeSelection>,
    pub(crate) filter: Option<FilterExpression>,
}
impl PostSelection {
    pub fn new(range: Option<RangeSelection>, filter: Option<FilterExpression>) -> Self {
        Self { range, filter }
    }
    /// Post filters precede dates, followed by the range. Nested jobs reset positions.
    pub fn apply<'a>(
        &self,
        input: BoxStream<'a, Result<ExtractionEvent>>,
        before: Option<i64>,
        after: Option<i64>,
        unordered: bool,
    ) -> BoxStream<'a, Result<Media>> {
        crate::job_selection::JobSelection {
            post_selection: Some(self.clone()),
            before,
            after,
            unordered,
            ..Default::default()
        }
        .apply(input)
    }
}
