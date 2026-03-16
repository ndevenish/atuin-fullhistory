use async_trait::async_trait;
use eyre::Result;

use crate::types::{Context, FilterMode, History, HistoryId, HistoryStats, OptFilters, SearchMode};

#[async_trait]
pub trait Db: Send + Sync + 'static {
    async fn load(&self, id: &str) -> Result<Option<History>>;

    async fn list(
        &self,
        filters: &[FilterMode],
        context: &Context,
        max: Option<usize>,
        unique: bool,
        include_deleted: bool,
    ) -> Result<Vec<History>>;

    async fn search(
        &self,
        search_mode: SearchMode,
        filter: FilterMode,
        context: &Context,
        query: &str,
        filter_options: OptFilters,
    ) -> Result<Vec<History>>;

    async fn all_with_count(&self) -> Result<Vec<(History, i32)>>;

    async fn history_count(&self, include_deleted: bool) -> Result<i64>;

    async fn stats(&self, h: &History) -> Result<HistoryStats>;

    async fn delete(&self, h: History) -> Result<()>;

    fn clone_boxed(&self) -> Box<dyn Db + 'static>;
}
