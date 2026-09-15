//! 用例层。
//!
//! **所有业务流程都住在这里**，CLI 和以后的 HTTP server 都只是薄包装。
//! 这是「v1 不做 Web 但以后不返工」的全部秘诀：等要加 Web 时，
//! server 只需要把这些函数包成 handler，不必重新实现任何流程。

use anyhow::{anyhow, Context, Result};
use tracing::info;

use crate::db;
use crate::filter::{render_for_prompt, CommentFilter};
use crate::model::{Analysis, Item, ItemKey, ListQuery};
use crate::ports::{AnalysisInput, Analyzer, Source};
use crate::Ctx;

/// `phi add <url>` 的主循环：抓取 → 评论 → 过滤 → 分析 → 落库。
pub async fn ingest_url(
    ctx: &Ctx,
    source: &dyn Source,
    analyzer: &dyn Analyzer,
    url: &str,
) -> Result<(Item, Analysis)> {
    let key = source.parse_url(url)?;
    let item_id = fetch_and_store(ctx, source, &key).await?;
    let analysis = analyze(ctx, analyzer, item_id).await?;
    let item = db::get_item(&ctx.db, item_id)
        .await?
        .ok_or_else(|| anyhow!("item {item_id} 入库后读不回来"))?;
    Ok((item, analysis))
}

/// 抓一条 item 连同它的评论，跑一遍预过滤后落库。返回 item id。
pub async fn fetch_and_store(ctx: &Ctx, source: &dyn Source, key: &ItemKey) -> Result<i64> {
    let new_item = source.fetch_one(key).await.context("抓取产品信息失败")?;
    let item_id = db::upsert_item(&ctx.db, &new_item).await?;
    info!(item_id, name = %new_item.name, "已入库");

    let comments = source
        .fetch_discussion(key)
        .await
        .context("抓取评论失败")?;

    let filter = CommentFilter::new(&ctx.config.comment_filter)?;
    let decisions = filter.apply(&comments);
    let kept = decisions.iter().filter(|d| d.kept).count();
    info!(total = comments.len(), kept, "评论预过滤完成");

    db::upsert_comments(&ctx.db, item_id, &comments, &decisions).await?;
    db::mark_comments_fetched(&ctx.db, item_id).await?;

    Ok(item_id)
}

/// 对一条已入库的 item 跑分析。**追加一条新的 analysis 记录**，不覆盖旧的。
pub async fn analyze(ctx: &Ctx, analyzer: &dyn Analyzer, item_id: i64) -> Result<Analysis> {
    let item = db::get_item(&ctx.db, item_id)
        .await?
        .ok_or_else(|| anyhow!("找不到 item {item_id}"))?;

    let comments = db::load_comments(&ctx.db, item_id, true).await?;
    if comments.is_empty() {
        return Err(anyhow!(
            "item {item_id}（{}）没有通过过滤的评论。\n\
             这个工具的整套分析框架建立在真实用户声音上 —— 没有评论的产品，\n\
             模型只能基于厂商的营销话术编，产出必然是废话。\n\
             要么换一个讨论多的产品，要么放宽 config.toml 里的 comment_filter。",
            item.name
        ));
    }

    let input = build_input(&item, &comments);
    let snapshot = serde_json::to_value(&input)?;

    info!(item_id, comments = comments.len(), model = analyzer.model(), "开始分析");
    let (card, usage) = analyzer.analyze(&input).await.context("模型分析失败")?;

    let analysis_id = db::insert_analysis(
        &ctx.db,
        item_id,
        analyzer.prompt_version(),
        analyzer.model(),
        &snapshot,
        &card,
        &usage,
    )
    .await?;

    db::get_analysis(&ctx.db, analysis_id)
        .await?
        .ok_or_else(|| anyhow!("analysis {analysis_id} 写入后读不回来"))
}

/// 用同样的输入快照重跑一次分析。
///
/// 刻意**不重新抓取** —— 保证换 prompt 或换模型时只有那一个变量在动。
/// 这是判断「prompt 改好了还是改坏了」的唯一可靠手段。
pub async fn reanalyze(
    ctx: &Ctx,
    analyzer: &dyn Analyzer,
    analysis_id: i64,
) -> Result<Analysis> {
    let prev = db::get_analysis(&ctx.db, analysis_id)
        .await?
        .ok_or_else(|| anyhow!("找不到 analysis {analysis_id}"))?;

    let row: (String,) = sqlx::query_as("SELECT input_snapshot FROM analysis WHERE id = ?1")
        .bind(analysis_id)
        .fetch_one(&ctx.db)
        .await?;
    let input: AnalysisInput = serde_json::from_str(&row.0)?;
    let snapshot = serde_json::to_value(&input)?;

    let (card, usage) = analyzer.analyze(&input).await.context("模型分析失败")?;

    let new_id = db::insert_analysis(
        &ctx.db,
        prev.item_id,
        analyzer.prompt_version(),
        analyzer.model(),
        &snapshot,
        &card,
        &usage,
    )
    .await?;

    db::get_analysis(&ctx.db, new_id)
        .await?
        .ok_or_else(|| anyhow!("analysis {new_id} 写入后读不回来"))
}

/// 阶段一：批量拉轻量元数据入库，**不碰评论、不做分析**。
///
/// 两阶段的动机是 PH 的复杂度配额（6250 points / 15 分钟），不是模型成本 ——
/// 模型那边已经便宜到可以忽略，但 PH 那堵墙绕不过去。
pub async fn sync(ctx: &Ctx, source: &dyn Source, q: &ListQuery) -> Result<usize> {
    let mut cursor: Option<String> = None;
    let mut total = 0usize;

    loop {
        let page = source.list(q, cursor.as_deref()).await?;
        for it in &page.items {
            db::upsert_item(&ctx.db, it).await?;
            total += 1;
        }
        info!(total, "sync 进行中");

        if q.limit > 0 && total >= q.limit {
            break;
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    Ok(total)
}

/// 阶段二：对通过阈值的候选拉评论。贵，所以只对候选做。
pub async fn hydrate(
    ctx: &Ctx,
    source: &dyn Source,
    min_signal: i64,
    limit: i64,
) -> Result<usize> {
    let items = db::list_items(&ctx.db, min_signal, limit, None).await?;
    let filter = CommentFilter::new(&ctx.config.comment_filter)?;
    let mut done = 0;

    for item in items {
        if item.comments_fetched_at.is_some() {
            continue;
        }
        let key = item
            .slug
            .clone()
            .map(ItemKey::Slug)
            .unwrap_or_else(|| ItemKey::SourceId(item.source_id.clone()));

        let comments = match source.fetch_discussion(&key).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(item_id = item.id, error = %e, "拉评论失败，跳过");
                continue;
            }
        };
        let decisions = filter.apply(&comments);
        db::upsert_comments(&ctx.db, item.id, &comments, &decisions).await?;
        db::mark_comments_fetched(&ctx.db, item.id).await?;
        done += 1;
    }

    Ok(done)
}

pub async fn add_note(ctx: &Ctx, item_id: i64, body: &str) -> Result<i64> {
    db::insert_note(&ctx.db, item_id, body).await
}

pub async fn search(ctx: &Ctx, q: &str, limit: i64) -> Result<Vec<Item>> {
    db::search_items(&ctx.db, q, limit).await
}

fn build_input(item: &Item, comments: &[crate::model::Comment]) -> AnalysisInput {
    AnalysisInput {
        name: item.name.clone(),
        tagline: item.tagline.clone().unwrap_or_default(),
        description: item.description.clone().unwrap_or_default(),
        website: item.website.clone().unwrap_or_default(),
        url: item.url.clone(),
        topics: item.topics.join(", "),
        posted_at: item.posted_at.clone().unwrap_or_default(),
        votes: item.vote_count,
        comments_count: item.signal_count,
        comments: render_for_prompt(comments),
    }
}
