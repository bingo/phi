//! 信息源适配器。
//!
//! v1 只有 ProductHunt。`Source` trait 定义在 `phi-core::ports`，所以加第二个源
//! （TechCrunch、36kr）时不需要动表结构，也不需要动用例层。
//!
//! 值得先想清楚的一点：本工具的整套分析框架建立在**真实用户评论**上。
//! RSS 类的新闻源没有评论区，质量不对等 —— 直接接进来会稀释卡片库。
//! 加新源时先回答「它的用户声音从哪来」，再写 adapter。

pub mod producthunt;

pub use producthunt::ProductHunt;
