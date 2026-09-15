//! HTTP API —— v0.4 的范围，现在是占位。
//!
//! 之所以现在就把 crate 建出来，是为了守住那条分层约束：**业务流程住在
//! `phi_core::usecase`，CLI 和 server 都只是薄包装**。等真要写的时候，
//! 这里只需要把用例函数包成 axum handler，不必重新实现任何流程。
//!
//! 计划中的只读端点：
//!
//! ```text
//! GET /items?min_comments=15&verdict=follow   → usecase::list
//! GET /items/:id                              → db::get_item + latest_analysis
//! GET /items/:id/analyses                     → db::analysis_history
//! GET /search?q=...                           → usecase::search
//! ```

/// 占位，避免空 crate 触发 warning。
pub fn planned_routes() -> &'static [&'static str] {
    &[
        "GET /items",
        "GET /items/:id",
        "GET /items/:id/analyses",
        "GET /search",
    ]
}
