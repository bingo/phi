//! AI 分析层。
//!
//! v1 只有 OpenRouter 一个实现。`Analyzer` trait 定义在 `phi-core::ports`。

pub mod openrouter;
pub mod prompt;
pub mod schema;

pub use openrouter::OpenRouterAnalyzer;
pub use prompt::PromptTemplate;
