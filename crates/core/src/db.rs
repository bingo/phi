//! SQLite 仓储。
//!
//! 刻意用运行期的 `sqlx::query`（而不是 `query!` 宏）：宏要求编译期能连上一个真实
//! 数据库，会让 `cargo check` 依赖 `DATABASE_URL`。对一个自用工具来说不值这个麻烦。

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;

use crate::model::{
    Analysis, Comment, Evidence, Item, NewComment, NewItem, OpportunityCard, Usage,
};

pub async fn connect(path: &Path) -> Result<SqlitePool> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .with_context(|| format!("打开数据库失败: {}", path.display()))?;

    sqlx::query("PRAGMA journal_mode = WAL").execute(&pool).await?;
    Ok(pool)
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .context("执行 migrations 失败")?;
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------- item

/// 插入或更新一条 item，返回其 id。
pub async fn upsert_item(pool: &SqlitePool, it: &NewItem) -> Result<i64> {
    let topics = serde_json::to_string(&it.topics)?;
    let raw = serde_json::to_string(&it.raw)?;

    let id: i64 = sqlx::query(
        r#"
        INSERT INTO item (source, source_id, slug, url, name, tagline, description,
                          website, posted_at, signal_count, vote_count, topics, raw, fetched_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT (source, source_id) DO UPDATE SET
            slug         = excluded.slug,
            url          = excluded.url,
            name         = excluded.name,
            tagline      = excluded.tagline,
            description  = excluded.description,
            website      = excluded.website,
            posted_at    = excluded.posted_at,
            signal_count = excluded.signal_count,
            vote_count   = excluded.vote_count,
            topics       = excluded.topics,
            raw          = excluded.raw,
            fetched_at   = excluded.fetched_at
        RETURNING id
        "#,
    )
    .bind(&it.source)
    .bind(&it.source_id)
    .bind(&it.slug)
    .bind(&it.url)
    .bind(&it.name)
    .bind(&it.tagline)
    .bind(&it.description)
    .bind(&it.website)
    .bind(&it.posted_at)
    .bind(it.signal_count)
    .bind(it.vote_count)
    .bind(&topics)
    .bind(&raw)
    .bind(now())
    .fetch_one(pool)
    .await?
    .get("id");

    Ok(id)
}

fn row_to_item(row: &sqlx::sqlite::SqliteRow) -> Result<Item> {
    let topics: Option<String> = row.try_get("topics")?;
    Ok(Item {
        id: row.try_get("id")?,
        source: row.try_get("source")?,
        source_id: row.try_get("source_id")?,
        slug: row.try_get("slug")?,
        url: row.try_get("url")?,
        name: row.try_get("name")?,
        tagline: row.try_get("tagline")?,
        description: row.try_get("description")?,
        website: row.try_get("website")?,
        posted_at: row.try_get("posted_at")?,
        signal_count: row.try_get("signal_count")?,
        vote_count: row.try_get("vote_count")?,
        topics: topics
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default(),
        fetched_at: row.try_get("fetched_at")?,
        comments_fetched_at: row.try_get("comments_fetched_at")?,
    })
}

pub async fn get_item(pool: &SqlitePool, id: i64) -> Result<Option<Item>> {
    let row = sqlx::query("SELECT * FROM item WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(row_to_item).transpose()
}

/// 列出条目。`min_signal` 就是「评论够多才值得深挖」那道可调阈值 ——
/// 它是**视图层的过滤**，不是采集时的硬筛：阈值选错了不用重拉数据。
pub async fn list_items(
    pool: &SqlitePool,
    min_signal: i64,
    limit: i64,
    analyzed: Option<bool>,
) -> Result<Vec<Item>> {
    let sql = match analyzed {
        None => {
            "SELECT * FROM item WHERE signal_count >= ?1 \
             ORDER BY signal_count DESC LIMIT ?2"
        }
        Some(true) => {
            "SELECT i.* FROM item i WHERE i.signal_count >= ?1 \
             AND EXISTS (SELECT 1 FROM analysis a WHERE a.item_id = i.id) \
             ORDER BY i.signal_count DESC LIMIT ?2"
        }
        Some(false) => {
            "SELECT i.* FROM item i WHERE i.signal_count >= ?1 \
             AND NOT EXISTS (SELECT 1 FROM analysis a WHERE a.item_id = i.id) \
             ORDER BY i.signal_count DESC LIMIT ?2"
        }
    };
    let rows = sqlx::query(sql)
        .bind(min_signal)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter().map(row_to_item).collect()
}

pub async fn mark_comments_fetched(pool: &SqlitePool, item_id: i64) -> Result<()> {
    sqlx::query("UPDATE item SET comments_fetched_at = ?1 WHERE id = ?2")
        .bind(now())
        .bind(item_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- comment

pub async fn upsert_comments(
    pool: &SqlitePool,
    item_id: i64,
    comments: &[NewComment],
    decisions: &[crate::filter::Decision],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    for (c, d) in comments.iter().zip(decisions) {
        sqlx::query(
            r#"
            INSERT INTO comment (item_id, source_comment_id, author, is_maker, body,
                                 votes, created_at, kept, filter_reason)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT (item_id, source_comment_id) DO UPDATE SET
                body          = excluded.body,
                votes         = excluded.votes,
                is_maker      = excluded.is_maker,
                kept          = excluded.kept,
                filter_reason = excluded.filter_reason
            "#,
        )
        .bind(item_id)
        .bind(&c.source_comment_id)
        .bind(&c.author)
        .bind(c.is_maker as i32)
        .bind(&c.body)
        .bind(c.votes)
        .bind(&c.created_at)
        .bind(d.kept as i32)
        .bind(&d.reason)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn load_comments(
    pool: &SqlitePool,
    item_id: i64,
    kept_only: bool,
) -> Result<Vec<Comment>> {
    let sql = if kept_only {
        "SELECT * FROM comment WHERE item_id = ?1 AND kept = 1 ORDER BY votes DESC"
    } else {
        "SELECT * FROM comment WHERE item_id = ?1 ORDER BY votes DESC"
    };
    let rows = sqlx::query(sql).bind(item_id).fetch_all(pool).await?;
    rows.iter()
        .map(|row| {
            Ok(Comment {
                id: row.try_get("id")?,
                item_id: row.try_get("item_id")?,
                source_comment_id: row.try_get("source_comment_id")?,
                author: row.try_get("author")?,
                is_maker: row.try_get::<i32, _>("is_maker")? != 0,
                body: row.try_get("body")?,
                votes: row.try_get("votes")?,
                created_at: row.try_get("created_at")?,
                kept: row.try_get::<i32, _>("kept")? != 0,
                filter_reason: row.try_get("filter_reason")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------- analysis

/// 写入一次分析。**追加，不覆盖** —— 旧版本保留才能做 diff。
#[allow(clippy::too_many_arguments)]
pub async fn insert_analysis(
    pool: &SqlitePool,
    item_id: i64,
    prompt_version: &str,
    model: &str,
    input_snapshot: &serde_json::Value,
    card: &OpportunityCard,
    usage: &Usage,
) -> Result<i64> {
    let mut tx = pool.begin().await?;

    let card_json = serde_json::to_string(card)?;
    let snapshot_json = serde_json::to_string(input_snapshot)?;

    let analysis_id: i64 = sqlx::query(
        r#"
        INSERT INTO analysis (item_id, created_at, prompt_version, model, provider,
                              input_snapshot, card, buildable, worth_it, reachable,
                              verdict, tokens_in, tokens_out, cost_usd)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        RETURNING id
        "#,
    )
    .bind(item_id)
    .bind(now())
    .bind(prompt_version)
    .bind(model)
    .bind(&usage.provider)
    .bind(&snapshot_json)
    .bind(&card_json)
    .bind(card.buildable.value.as_str())
    .bind(card.worth_it.value.as_str())
    .bind(card.reachable.value.as_str())
    .bind(card.verdict.as_str())
    .bind(usage.tokens_in)
    .bind(usage.tokens_out)
    .bind(usage.cost_usd)
    .fetch_one(&mut *tx)
    .await?
    .get("id");

    let push = |field: &str, ev: &[Evidence]| -> Vec<(String, String, String)> {
        ev.iter()
            .map(|e| (field.to_string(), e.quote.clone(), e.source_ref.clone()))
            .collect()
    };
    let mut all = push("pain", &card.pain_evidence);
    all.extend(push("who_pays", &card.who_pays_evidence));
    all.extend(push("gap", &card.gap_evidence));

    for (field, quote, source_ref) in all {
        sqlx::query(
            "INSERT INTO evidence (analysis_id, field, quote, source_ref) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(analysis_id)
        .bind(field)
        .bind(quote)
        .bind(source_ref)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("INSERT INTO card_fts (rowid, card_text) VALUES (?1, ?2)")
        .bind(analysis_id)
        .bind(card.to_search_text())
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(analysis_id)
}

fn row_to_analysis(row: &sqlx::sqlite::SqliteRow) -> Result<Analysis> {
    let card_json: String = row.try_get("card")?;
    Ok(Analysis {
        id: row.try_get("id")?,
        item_id: row.try_get("item_id")?,
        created_at: row.try_get("created_at")?,
        prompt_version: row.try_get("prompt_version")?,
        model: row.try_get("model")?,
        provider: row.try_get("provider")?,
        card: serde_json::from_str(&card_json)?,
        tokens_in: row.try_get("tokens_in")?,
        tokens_out: row.try_get("tokens_out")?,
        cost_usd: row.try_get("cost_usd")?,
    })
}

pub async fn get_analysis(pool: &SqlitePool, id: i64) -> Result<Option<Analysis>> {
    let row = sqlx::query("SELECT * FROM analysis WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(row_to_analysis).transpose()
}

pub async fn latest_analysis(pool: &SqlitePool, item_id: i64) -> Result<Option<Analysis>> {
    let row = sqlx::query(
        "SELECT * FROM analysis WHERE item_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(row_to_analysis).transpose()
}

/// 取某条 item 的全部分析版本，新的在前。换 prompt / 换模型重跑后用它做对比。
pub async fn analysis_history(pool: &SqlitePool, item_id: i64) -> Result<Vec<Analysis>> {
    let rows = sqlx::query(
        "SELECT * FROM analysis WHERE item_id = ?1 ORDER BY created_at DESC, id DESC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_analysis).collect()
}

// ---------------------------------------------------------------- note

/// 你的笔记。独立一条线 —— 分析可以删了重来，笔记不受影响。
pub async fn insert_note(pool: &SqlitePool, item_id: i64, body: &str) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO note (item_id, created_at, body) VALUES (?1, ?2, ?3) RETURNING id",
    )
    .bind(item_id)
    .bind(now())
    .bind(body)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn load_notes(pool: &SqlitePool, item_id: i64) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT created_at, body FROM note WHERE item_id = ?1 ORDER BY created_at DESC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("created_at")?, r.try_get("body")?)))
        .collect()
}

// ---------------------------------------------------------------- search

pub async fn search_items(pool: &SqlitePool, q: &str, limit: i64) -> Result<Vec<Item>> {
    let rows = sqlx::query(
        "SELECT i.* FROM item i JOIN item_fts f ON f.rowid = i.id \
         WHERE item_fts MATCH ?1 ORDER BY rank LIMIT ?2",
    )
    .bind(q)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_item).collect()
}
