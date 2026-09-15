//! 对外依赖的两个 trait。
//!
//! 定义在 core 而不是各自的 crate 里，这样 `sources` / `analyzer` 依赖 `core`，
//! 而 `core` 不依赖它们 —— 没有循环，用例层也能只面向 trait 编程。

use anyhow::Result;
use async_trait::async_trait;

use crate::model::{ItemKey, ListQuery, NewComment, NewItem, Page, Usage};

/// 一个信息源。v1 只实现 ProductHunt，但 schema 和用例层都已经是中性的，
/// 加 TechCrunch / 36kr 时不用动表结构。
#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &'static str;

    /// 能否处理这个 URL。CLI 用它在多个源之间派发。
    fn matches_url(&self, url: &str) -> bool;

    /// 把一个页面 URL 解析成源站内的定位键。
    fn parse_url(&self, url: &str) -> Result<ItemKey>;

    /// 阶段一：批量拉轻量元数据（含讨论热度），不碰评论。
    /// 便宜，可大批量 —— PH 的复杂度配额撑得住。
    async fn list(&self, q: &ListQuery, cursor: Option<&str>) -> Result<Page<NewItem>>;

    /// 单条抓取。
    async fn fetch_one(&self, key: &ItemKey) -> Result<NewItem>;

    /// 阶段二：拉讨论。贵，只对通过门槛的候选做。
    async fn fetch_discussion(&self, key: &ItemKey) -> Result<Vec<NewComment>>;
}

/// 喂给模型的完整输入。会被原样存进 `analysis.input_snapshot`，
/// 这样换 prompt 或换模型重跑时能保证只有那一个变量在动。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnalysisInput {
    pub name: String,
    pub tagline: String,
    pub description: String,
    pub website: String,
    pub url: String,
    pub topics: String,
    pub posted_at: String,
    pub votes: i64,
    pub comments_count: i64,
    /// 已过滤、已排序、已带 `[votes=N]` / `[maker]` 标记的评论块。
    pub comments: String,
}

#[async_trait]
pub trait Analyzer: Send + Sync {
    /// 模型标识，写进 `analysis.model`。
    fn model(&self) -> &str;
    /// prompt 模板版本，写进 `analysis.prompt_version`。
    fn prompt_version(&self) -> &str;

    async fn analyze(
        &self,
        input: &AnalysisInput,
    ) -> Result<(crate::model::OpportunityCard, Usage)>;
}
