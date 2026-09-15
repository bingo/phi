//! phi 的领域内核。
//!
//! 这里放：领域模型、`Source` / `Analyzer` 两个 trait、SQLite 仓储、评论预过滤、
//! 卡片渲染，以及用例层。
//!
//! 关键的分层约束：**用例函数写在 `usecase`，不写在 CLI**。CLI 和以后的 HTTP server
//! 都只是这些函数的薄包装。这是「v1 不做 Web 但以后不返工」的全部秘诀。
//!
//! 两个 trait 也定义在这里（而不是各自的 crate 里），这样 `sources` 和 `analyzer`
//! 依赖 `core`，而 `core` 不依赖它们 —— 没有循环。

pub mod config;
pub mod db;
pub mod filter;
pub mod model;
pub mod ports;
pub mod render;
pub mod usecase;

pub use config::Config;
pub use ports::{Analyzer, Source};

/// 用例层共享的运行期上下文。`Clone` 很便宜（连接池内部是 Arc），后台任务各持一份。
#[derive(Clone)]
pub struct Ctx {
    pub config: Config,
    pub db: sqlx::SqlitePool,
}

impl Ctx {
    pub fn new(config: Config, db: sqlx::SqlitePool) -> Self {
        Self { config, db }
    }
}
