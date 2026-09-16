//! 用例层。
//!
//! **所有业务流程都住在这里**，CLI 和以后的 HTTP server 都只是薄包装。
//! 这是「v1 不做 Web 但以后不返工」的全部秘诀：等要加 Web 时，
//! server 只需要把这些函数包成 handler，不必重新实现任何流程。

use anyhow::{anyhow, Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use std::collections::HashSet;
use tracing::info;

use crate::db;
use crate::filter::{render_for_prompt, CommentFilter};
use crate::model::{
    Analysis, AnalysisState, Item, ItemDetail, ItemKey, ItemSummary, ListQuery, Overview,
    OverviewQuery, SortKey, SyncReport, SyncRequest,
};
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

    let comments = source.fetch_discussion(key).await.context("抓取评论失败")?;

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

    info!(
        item_id,
        comments = comments.len(),
        model = analyzer.model(),
        "开始分析"
    );
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
pub async fn reanalyze(ctx: &Ctx, analyzer: &dyn Analyzer, analysis_id: i64) -> Result<Analysis> {
    let prev = db::get_analysis(&ctx.db, analysis_id)
        .await?
        .ok_or_else(|| anyhow!("找不到 analysis {analysis_id}"))?;

    let snapshot = db::get_input_snapshot(&ctx.db, analysis_id)
        .await?
        .ok_or_else(|| anyhow!("analysis {analysis_id} 没有输入快照"))?;
    let input: AnalysisInput = serde_json::from_str(&snapshot)?;
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
/// 两阶段的动机是 PH 的配额（每个请求固定 100 点，6250 点 / 15 分钟），不是模型成本。
///
/// # 为什么按天切片
///
/// 源站的列表是偏移量分页（PH 的游标就是 base64 编码的偏移），整段日期一把翻有两个问题：
/// 默认排序会把当天的产品排在最前，`--limit` 永远只够覆盖「今天」；而且排序在一天之内会变，
/// 偏移量续传会跳条或重复。按 UTC 自然日切片后，每一天是一个独立的小窗口：
///
/// - **已经结束的一天**，产品集合是固定的，偏移量可靠 —— 每翻一页存一次游标，翻完标记完成，
///   之后的 sync 直接跳过（`refresh` 除外）
/// - **还没结束的一天**（通常是今天），新发布的产品会把偏移量往后推 —— 不存游标、不标记完成，
///   每次都从头拉
pub async fn sync(ctx: &Ctx, source: &dyn Source, req: &SyncRequest) -> Result<SyncReport> {
    if req.since > req.until {
        return Err(anyhow!("起始日期 {} 晚于结束日期 {}", req.since, req.until));
    }
    let started = Utc::now();
    let mut report = SyncReport {
        days: (req.until - req.since).num_days() as usize + 1,
        ..Default::default()
    };

    let mut day = req.since;
    while day <= req.until {
        let day_start = Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).unwrap());
        let day_end = day_start + chrono::Duration::days(1);
        let closed = day_end <= started;
        let key = sync_key(req.topic.as_deref(), day);

        let saved = db::get_sync_cursor(&ctx.db, source.id(), &key).await?;
        if saved.completed_at.is_some() && !req.refresh {
            report.days_skipped += 1;
            day = day.succ_opt().unwrap();
            continue;
        }
        // 预算已经用完：后面的日子不再发请求，只是继续往下走一遍，把已完成的算进「跳过」，
        // 这样报告里的「未拉完」才准确
        if report.budget_exhausted {
            day = day.succ_opt().unwrap();
            continue;
        }
        let mut cursor = if closed && !req.refresh {
            saved.cursor
        } else {
            None
        };
        if cursor.is_some() {
            info!(%day, "从上次中断的位置续传");
        }

        let q = ListQuery {
            topic: req.topic.clone(),
            posted_after: Some(day_start),
            posted_before: Some(day_end),
        };
        let (mut day_pages, mut day_new) = (0usize, 0usize);

        loop {
            if req.max_pages.is_some_and(|m| report.pages >= m) {
                report.budget_exhausted = true;
                break;
            }
            let page = source.list(&q, cursor.as_deref()).await.with_context(|| {
                format!(
                    "拉取 {day} 第 {} 页失败。已完成的页进度已保存，重跑会续传",
                    day_pages + 1
                )
            })?;
            report.pages += 1;
            day_pages += 1;

            for it in &page.items {
                let (_, inserted) = db::upsert_item_tracked(&ctx.db, it).await?;
                if inserted {
                    report.inserted += 1;
                    day_new += 1;
                } else {
                    report.updated += 1;
                }
            }

            cursor = page.next_cursor;
            let done = cursor.is_none();
            if closed {
                db::save_sync_cursor(&ctx.db, source.id(), &key, cursor.as_deref(), done).await?;
            }
            if done {
                if closed {
                    report.days_completed += 1;
                } else {
                    report.days_open += 1;
                }
                info!(%day, pages = day_pages, new = day_new, closed, "这一天拉完了");
                break;
            }
            if day_pages % 10 == 0 {
                info!(%day, pages = day_pages, new = day_new, "sync 进行中");
            }
        }

        day = day.succ_opt().unwrap();
    }

    Ok(report)
}

/// sync 游标的键。topic 不同就是不同的切片 —— 按 topic 拉完不代表全量拉完。
fn sync_key(topic: Option<&str>, day: NaiveDate) -> String {
    format!("posts|topic={}|day={day}", topic.unwrap_or("*"))
}

/// 阶段二：对通过阈值的候选拉评论。贵，所以只对候选做。
pub async fn hydrate(ctx: &Ctx, source: &dyn Source, min_signal: i64, limit: i64) -> Result<usize> {
    let items = db::list_items(&ctx.db, min_signal, limit, None).await?;
    let filter = CommentFilter::new(&ctx.config.comment_filter)?;
    let mut done = 0;

    for item in items {
        if item.comments_fetched_at.is_some() {
            continue;
        }
        match fetch_comments_for(ctx, source, &filter, &item).await {
            Ok(_) => done += 1,
            Err(e) => tracing::warn!(item_id = item.id, error = %e, "拉评论失败，跳过"),
        }
    }

    Ok(done)
}

/// 给一条已入库的 item 抓评论、预过滤、落库。返回抓到的评论条数。
async fn fetch_comments_for(
    ctx: &Ctx,
    source: &dyn Source,
    filter: &CommentFilter,
    item: &Item,
) -> Result<usize> {
    let key = item
        .slug
        .clone()
        .map(ItemKey::Slug)
        .unwrap_or_else(|| ItemKey::SourceId(item.source_id.clone()));
    let comments = source.fetch_discussion(&key).await?;
    let decisions = filter.apply(&comments);
    db::upsert_comments(&ctx.db, item.id, &comments, &decisions).await?;
    db::mark_comments_fetched(&ctx.db, item.id).await?;
    Ok(comments.len())
}

/// [`analyze_item`] 进行到哪一步。给需要进度提示的调用方（TUI）用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzeStage {
    FetchingComments,
    CallingModel,
}

/// 对一条已入库的 item 做完整分析：**评论还没抓过就先抓**（`phi sync` 入库的条目只有元数据），
/// 再调用模型。和 [`analyze`] 一样追加一条新的 analysis，不覆盖旧的。
///
/// `source` 只在需要抓评论时用到；评论已经抓过的条目可以传 `None`。
pub async fn analyze_item(
    ctx: &Ctx,
    source: Option<&dyn Source>,
    analyzer: &dyn Analyzer,
    item_id: i64,
    on_stage: &(dyn Fn(AnalyzeStage) + Send + Sync),
) -> Result<Analysis> {
    let item = db::get_item(&ctx.db, item_id)
        .await?
        .ok_or_else(|| anyhow!("找不到 item {item_id}"))?;

    if item.comments_fetched_at.is_none() {
        let source = source.ok_or_else(|| {
            anyhow!(
                "「{}」还没抓过评论，需要先抓 —— 但信息源没配置好（缺 PH_TOKEN？）",
                item.name
            )
        })?;
        on_stage(AnalyzeStage::FetchingComments);
        let filter = CommentFilter::new(&ctx.config.comment_filter)?;
        let n = fetch_comments_for(ctx, source, &filter, &item)
            .await
            .context("抓取评论失败")?;
        info!(item_id, comments = n, "评论抓取完成");
    }

    on_stage(AnalyzeStage::CallingModel);
    analyze(ctx, analyzer, item_id).await
}

pub async fn add_note(ctx: &Ctx, item_id: i64, body: &str) -> Result<i64> {
    db::insert_note(&ctx.db, item_id, body).await
}

pub async fn search(ctx: &Ctx, q: &str, limit: i64) -> Result<Vec<Item>> {
    db::search_items(&ctx.db, q, limit).await
}

/// 浏览视图：按条件筛选、排序后的列表。
pub async fn overview(ctx: &Ctx, q: &OverviewQuery) -> Result<Overview> {
    let rows = db::list_summaries(&ctx.db).await?;
    let total = rows.len();
    let matching = match q.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => Some(
            db::item_ids_containing(&ctx.db, t)
                .await?
                .into_iter()
                .collect(),
        ),
        None => None,
    };
    Ok(Overview {
        rows: select_overview(rows, q, matching.as_ref()),
        total,
    })
}

/// [`overview`] 的纯函数部分。`matching` 是文本搜索命中的 id 集合，`None` 表示没有搜索。
pub fn select_overview(
    rows: Vec<ItemSummary>,
    q: &OverviewQuery,
    matching: Option<&HashSet<i64>>,
) -> Vec<ItemSummary> {
    let mut rows: Vec<ItemSummary> = rows
        .into_iter()
        .filter(|r| r.item.signal_count >= q.min_signal)
        .filter(|r| match q.state {
            AnalysisState::All => true,
            AnalysisState::Analyzed => r.latest_analysis_id.is_some(),
            AnalysisState::Pending => r.latest_analysis_id.is_none(),
        })
        .filter(|r| q.verdict.is_none() || r.verdict == q.verdict)
        .filter(|r| q.buildable.is_none() || r.buildable == q.buildable)
        .filter(|r| q.worth_it.is_none() || r.worth_it == q.worth_it)
        .filter(|r| q.reachable.is_none() || r.reachable == q.reachable)
        .filter(|r| matching.is_none_or(|m| m.contains(&r.item.id)))
        .collect();

    match q.sort {
        SortKey::Comments => rows.sort_by(|a, b| {
            (b.item.signal_count, b.item.vote_count).cmp(&(a.item.signal_count, a.item.vote_count))
        }),
        SortKey::Votes => rows.sort_by_key(|r| std::cmp::Reverse(r.item.vote_count)),
        // RFC 3339 字符串按字典序比较即按时间比较；没有发布时间的排最后
        SortKey::Newest => rows.sort_by(|a, b| b.item.posted_at.cmp(&a.item.posted_at)),
        SortKey::Name => rows.sort_by_key(|r| r.item.name.to_lowercase()),
    }
    rows
}

/// 一条 item 的全部分析版本和全部笔记。
pub async fn item_detail(ctx: &Ctx, item_id: i64) -> Result<ItemDetail> {
    let item = db::get_item(&ctx.db, item_id)
        .await?
        .ok_or_else(|| anyhow!("找不到 item {item_id}"))?;
    Ok(ItemDetail {
        item,
        analyses: db::analysis_history(&ctx.db, item_id).await?,
        notes: db::load_notes(&ctx.db, item_id).await?,
    })
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
