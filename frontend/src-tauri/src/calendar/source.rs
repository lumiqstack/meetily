use super::types::{CalendarEvent, SourceHealth, SourceKind};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// One poll of a source. `events` is `None` when the source could not answer
/// and the caller should keep the previous list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePoll {
    pub health: SourceHealth,
    pub events: Option<Vec<CalendarEvent>>,
}

#[async_trait]
pub trait CalendarSource: Send + Sync {
    fn kind(&self) -> SourceKind;

    /// Events overlapping `[from, to)`, with recurring series expanded.
    async fn poll(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> SourcePoll;
}
