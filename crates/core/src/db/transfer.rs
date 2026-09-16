//! 两个后端之间搬数据。**只增，不改，不删。**
//!
//! 目标库里已经有的行一律原样放过（哪怕内容不同），目标库里多出来的行一律不动。
//! 所以它是幂等的：同一条命令跑两次，第二次什么都不会写。
//!
//! # 为什么不能直接 INSERT ... SELECT
//!
//! `item` / `analysis` / `thesis` 的主键是**自增**的，两个库里同一条数据的 id 几乎必然不同。
//! 子表（`comment` / `evidence` / `note` / `thesis_link`）存的是那些 id，照搬过去就会挂到
//! 错误的父行上。所以每张父表都要先按**自然键**在目标库里定位或新建，拿到目标 id，
//! 再用这份映射改写子表的外键。自然键：
//!
//! | 表 | 自然键 | 库里有约束吗 |
//! |---|---|---|
//! | `item` | `(source, source_id)` | 有（UNIQUE） |
//! | `comment` | `(item_id, source_comment_id)` | 有（UNIQUE） |
//! | `analysis` | `(item_id, created_at, prompt_version, model)` | 没有 —— 靠 `created_at` 的纳秒精度 |
//! | `note` | `(item_id, created_at)` | 没有，同上 |
//! | `sync_cursor` | `(source, query_key)` | 有（PK） |
//! | `thesis` | `(title, created_at)` | 没有 |
//! | `thesis_link` | `(thesis_id, item_id)` | 有（PK） |
//!
//! `evidence` 和 `card_fts` 没有任何可用的身份 —— 它们只是某次 analysis 的附属物，
//! 所以只在那次 analysis 是**新插入**的时候才跟着复制。已存在的 analysis 连带它的证据
//! 一起原样不动，否则重复跑就会把证据翻倍。
//!
//! # 量级假设
//!
//! 全表读进内存、每张表一个事务、逐行 INSERT。这是照自用工具的量级（几千行）定的。
//! 如果哪天数据涨到几十万行，要改成多值 INSERT 和分块提交。

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

use super::Db;
use sqlx::{ColumnIndex, Decode, Row, Type};

// ---------------------------------------------------------------- 报告

#[derive(Debug, Default, Clone, Copy)]
pub struct Tally {
    /// 目标库里没有、这次插进去的行数
    pub inserted: usize,
    /// 目标库里已经有、原样放过的行数
    pub skipped: usize,
}

impl Tally {
    fn total(&self) -> usize {
        self.inserted + self.skipped
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    pub items: Tally,
    pub comments: Tally,
    pub analyses: Tally,
    pub evidence: Tally,
    pub card_fts: Tally,
    pub notes: Tally,
    pub cursors: Tally,
    pub theses: Tally,
    pub thesis_links: Tally,
    /// 源库里外键指向了不存在的父行（正常情况下应当是 0）
    pub orphans: usize,
}

impl Report {
    /// 按表列出来，`表名 新增/共计`。
    pub fn lines(&self) -> Vec<String> {
        [
            ("item", self.items),
            ("comment", self.comments),
            ("analysis", self.analyses),
            ("evidence", self.evidence),
            ("card_fts", self.card_fts),
            ("note", self.notes),
            ("sync_cursor", self.cursors),
            ("thesis", self.theses),
            ("thesis_link", self.thesis_links),
        ]
        .iter()
        .map(|(name, t)| {
            format!(
                "{name:<12} 新增 {:>6}   已存在 {:>6}   源库共 {:>6}",
                t.inserted,
                t.skipped,
                t.total()
            )
        })
        .collect()
    }

    pub fn inserted_total(&self) -> usize {
        self.items.inserted
            + self.comments.inserted
            + self.analyses.inserted
            + self.evidence.inserted
            + self.card_fts.inserted
            + self.notes.inserted
            + self.cursors.inserted
            + self.theses.inserted
            + self.thesis_links.inserted
    }
}

// ---------------------------------------------------------------- 行

/// 源库读出来的一行 `item`。`id` 是**源库的** id，只用来给子表做映射。
struct ItemRow {
    id: i64,
    source: String,
    source_id: String,
    slug: Option<String>,
    url: String,
    name: String,
    tagline: Option<String>,
    description: Option<String>,
    website: Option<String>,
    posted_at: Option<String>,
    signal_count: i64,
    vote_count: i64,
    topics: Option<String>,
    raw: String,
    fetched_at: String,
    comments_fetched_at: Option<String>,
}

struct CommentRow {
    item_id: i64,
    source_comment_id: String,
    author: Option<String>,
    is_maker: i64,
    body: String,
    votes: i64,
    created_at: Option<String>,
    kept: i64,
    filter_reason: Option<String>,
}

struct AnalysisRow {
    id: i64,
    item_id: i64,
    created_at: String,
    prompt_version: String,
    model: String,
    provider: Option<String>,
    input_snapshot: String,
    card: String,
    buildable: Option<String>,
    worth_it: Option<String>,
    reachable: Option<String>,
    verdict: Option<String>,
    usefulness: Option<i64>,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    cost_usd: Option<f64>,
}

struct EvidenceRow {
    analysis_id: i64,
    field: String,
    quote: String,
    source_ref: Option<String>,
}

struct NoteRow {
    item_id: i64,
    created_at: String,
    body: String,
}

struct CursorRow {
    source: String,
    query_key: String,
    cursor: Option<String>,
    updated_at: String,
    completed_at: Option<String>,
}

struct ThesisRow {
    id: i64,
    title: String,
    body: Option<String>,
    status: String,
    created_at: String,
}

struct ThesisLinkRow {
    thesis_id: i64,
    item_id: i64,
    stance: String,
}

row_mapper!(fn map_item(r) -> ItemRow {
    Ok(ItemRow {
        id: r.try_get("id")?,
        source: r.try_get("source")?,
        source_id: r.try_get("source_id")?,
        slug: r.try_get("slug")?,
        url: r.try_get("url")?,
        name: r.try_get("name")?,
        tagline: r.try_get("tagline")?,
        description: r.try_get("description")?,
        website: r.try_get("website")?,
        posted_at: r.try_get("posted_at")?,
        signal_count: r.try_get("signal_count")?,
        vote_count: r.try_get("vote_count")?,
        topics: r.try_get("topics")?,
        raw: r.try_get("raw")?,
        fetched_at: r.try_get("fetched_at")?,
        comments_fetched_at: r.try_get("comments_fetched_at")?,
    })
});

row_mapper!(fn map_comment(r) -> CommentRow {
    Ok(CommentRow {
        item_id: r.try_get("item_id")?,
        source_comment_id: r.try_get("source_comment_id")?,
        author: r.try_get("author")?,
        is_maker: r.try_get("is_maker")?,
        body: r.try_get("body")?,
        votes: r.try_get("votes")?,
        created_at: r.try_get("created_at")?,
        kept: r.try_get("kept")?,
        filter_reason: r.try_get("filter_reason")?,
    })
});

row_mapper!(fn map_analysis(r) -> AnalysisRow {
    Ok(AnalysisRow {
        id: r.try_get("id")?,
        item_id: r.try_get("item_id")?,
        created_at: r.try_get("created_at")?,
        prompt_version: r.try_get("prompt_version")?,
        model: r.try_get("model")?,
        provider: r.try_get("provider")?,
        input_snapshot: r.try_get("input_snapshot")?,
        card: r.try_get("card")?,
        buildable: r.try_get("buildable")?,
        worth_it: r.try_get("worth_it")?,
        reachable: r.try_get("reachable")?,
        verdict: r.try_get("verdict")?,
        usefulness: r.try_get("usefulness")?,
        tokens_in: r.try_get("tokens_in")?,
        tokens_out: r.try_get("tokens_out")?,
        cost_usd: r.try_get("cost_usd")?,
    })
});

row_mapper!(fn map_evidence(r) -> EvidenceRow {
    Ok(EvidenceRow {
        analysis_id: r.try_get("analysis_id")?,
        field: r.try_get("field")?,
        quote: r.try_get("quote")?,
        source_ref: r.try_get("source_ref")?,
    })
});

row_mapper!(fn map_note(r) -> NoteRow {
    Ok(NoteRow {
        item_id: r.try_get("item_id")?,
        created_at: r.try_get("created_at")?,
        body: r.try_get("body")?,
    })
});

row_mapper!(fn map_cursor(r) -> CursorRow {
    Ok(CursorRow {
        source: r.try_get("source")?,
        query_key: r.try_get("query_key")?,
        cursor: r.try_get("cursor")?,
        updated_at: r.try_get("updated_at")?,
        completed_at: r.try_get("completed_at")?,
    })
});

row_mapper!(fn map_thesis(r) -> ThesisRow {
    Ok(ThesisRow {
        id: r.try_get("id")?,
        title: r.try_get("title")?,
        body: r.try_get("body")?,
        status: r.try_get("status")?,
        created_at: r.try_get("created_at")?,
    })
});

row_mapper!(fn map_thesis_link(r) -> ThesisLinkRow {
    Ok(ThesisLinkRow {
        thesis_id: r.try_get("thesis_id")?,
        item_id: r.try_get("item_id")?,
        stance: r.try_get("stance")?,
    })
});

/// `card_fts` 只有两列，直接当元组读。`rowid` 是保留字风险最低的写法：加反引号两边都认。
const SELECT_CARD_FTS: &str = "SELECT `rowid`, card_text FROM card_fts";

// ---------------------------------------------------------------- 主流程

/// 把 `from` 的数据搬进 `to`。只插入 `to` 里没有的行。
///
/// `dry_run = true` 时把该插的都算出来但一行都不写 —— 用来先看清楚会动多少数据。
pub async fn run(from: &Db, to: &Db, dry_run: bool) -> Result<Report> {
    let mut rep = Report {
        dry_run,
        ..Default::default()
    };

    // ---- item ----
    let src_items: Vec<ItemRow> = fetch_all_map!(from, "SELECT * FROM item", map_item);
    let mut tgt_items = item_keys(to).await?;
    {
        let sql = "INSERT INTO item (source, source_id, slug, url, name, tagline, description, \
                   website, posted_at, signal_count, vote_count, topics, raw, fetched_at, \
                   comments_fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        let todo: Vec<&ItemRow> = src_items
            .iter()
            .filter(|r| !tgt_items.contains_key(&(r.source.clone(), r.source_id.clone())))
            .collect();
        rep.items.skipped = src_items.len() - todo.len();
        rep.items.inserted = todo.len();
        if !dry_run && !todo.is_empty() {
            transaction!(to, |tx| {
                for r in &todo {
                    bind_all!(
                        sqlx::query(sql),
                        &r.source,
                        &r.source_id,
                        &r.slug,
                        &r.url,
                        &r.name,
                        &r.tagline,
                        &r.description,
                        &r.website,
                        &r.posted_at,
                        r.signal_count,
                        r.vote_count,
                        &r.topics,
                        &r.raw,
                        &r.fetched_at,
                        &r.comments_fetched_at,
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            });
            // 重新读一遍拿新 id：比逐行问一次 last_insert_id 少几千个来回
            tgt_items = item_keys(to).await?;
        }
        info!(
            inserted = rep.items.inserted,
            skipped = rep.items.skipped,
            "item"
        );
    }

    let item_map = parent_map(
        &src_items,
        |r| r.id,
        |r| (r.source.clone(), r.source_id.clone()),
        &tgt_items,
        dry_run,
        "item",
    )?;

    // ---- comment ----
    {
        let src: Vec<CommentRow> = fetch_all_map!(from, "SELECT * FROM comment", map_comment);
        let have = comment_keys(to).await?;
        let sql = "INSERT INTO comment (item_id, source_comment_id, author, is_maker, body, \
                   votes, created_at, kept, filter_reason) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
        let mut todo: Vec<(i64, &CommentRow)> = Vec::new();
        for r in &src {
            match item_map.get(&r.item_id) {
                None => rep.orphans += 1,
                Some(&tid) if have.contains(&(tid, r.source_comment_id.clone())) => {
                    rep.comments.skipped += 1
                }
                Some(&tid) => todo.push((tid, r)),
            }
        }
        rep.comments.inserted = todo.len();
        if !dry_run && !todo.is_empty() {
            transaction!(to, |tx| {
                for (tid, r) in &todo {
                    bind_all!(
                        sqlx::query(sql),
                        *tid,
                        &r.source_comment_id,
                        &r.author,
                        r.is_maker,
                        &r.body,
                        r.votes,
                        &r.created_at,
                        r.kept,
                        &r.filter_reason,
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            });
        }
        info!(
            inserted = rep.comments.inserted,
            skipped = rep.comments.skipped,
            "comment"
        );
    }

    // ---- analysis（+ 新增那些的 evidence / card_fts）----
    {
        let src: Vec<AnalysisRow> = fetch_all_map!(from, "SELECT * FROM analysis", map_analysis);
        let have = analysis_keys(to).await?;
        let sql = "INSERT INTO analysis (item_id, created_at, prompt_version, model, provider, \
                   input_snapshot, card, buildable, worth_it, reachable, verdict, usefulness, \
                   tokens_in, tokens_out, cost_usd) \
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        let mut todo: Vec<(i64, &AnalysisRow)> = Vec::new();
        for r in &src {
            match item_map.get(&r.item_id) {
                None => rep.orphans += 1,
                Some(&tid) if have.contains(&analysis_key(tid, r)) => rep.analyses.skipped += 1,
                Some(&tid) => todo.push((tid, r)),
            }
        }
        rep.analyses.inserted = todo.len();

        // evidence / card_fts 只跟着**新插入**的 analysis 走。已存在的 analysis 连它的证据
        // 一起原样不动 —— 否则重复跑会把证据翻倍（evidence 没有自然键可去重）
        let src_evidence: Vec<EvidenceRow> =
            fetch_all_map!(from, "SELECT * FROM evidence", map_evidence);
        let mut by_analysis: HashMap<i64, Vec<&EvidenceRow>> = HashMap::new();
        for e in &src_evidence {
            by_analysis.entry(e.analysis_id).or_default().push(e);
        }
        let src_fts: HashMap<i64, String> = fetch_all_as!(from, (i64, String), SELECT_CARD_FTS)
            .into_iter()
            .collect();

        if dry_run {
            for (_, r) in &todo {
                rep.evidence.inserted += by_analysis.get(&r.id).map_or(0, |v| v.len());
                rep.card_fts.inserted += src_fts.contains_key(&r.id) as usize;
            }
            rep.evidence.skipped = src_evidence.len() - rep.evidence.inserted;
            rep.card_fts.skipped = src_fts.len() - rep.card_fts.inserted;
        } else if !todo.is_empty() {
            transaction!(to, |tx| {
                for (tid, r) in &todo {
                    bind_all!(
                        sqlx::query(sql),
                        *tid,
                        &r.created_at,
                        &r.prompt_version,
                        &r.model,
                        &r.provider,
                        &r.input_snapshot,
                        &r.card,
                        &r.buildable,
                        &r.worth_it,
                        &r.reachable,
                        &r.verdict,
                        r.usefulness,
                        r.tokens_in,
                        r.tokens_out,
                        r.cost_usd,
                    )
                    .execute(&mut *tx)
                    .await?;
                    let new_id: i64 = sqlx::query_as::<_, (i64,)>(to.last_id_sql())
                        .fetch_one(&mut *tx)
                        .await?
                        .0;
                    for e in by_analysis.get(&r.id).into_iter().flatten() {
                        sqlx::query(
                            "INSERT INTO evidence (analysis_id, field, quote, source_ref) \
                             VALUES (?, ?, ?, ?)",
                        )
                        .bind(new_id)
                        .bind(&e.field)
                        .bind(&e.quote)
                        .bind(&e.source_ref)
                        .execute(&mut *tx)
                        .await?;
                    }
                    match src_fts.get(&r.id) {
                        Some(text) => {
                            sqlx::query("INSERT INTO card_fts (`rowid`, card_text) VALUES (?, ?)")
                                .bind(new_id)
                                .bind(text)
                                .execute(&mut *tx)
                                .await?;
                        }
                        None => warn!(
                            src_analysis = r.id,
                            "源库缺 card_fts 行，目标库这条分析搜不到正文"
                        ),
                    }
                }
            });
            // 事务里数不方便，统计在外面按同一套条件重算
            for (_, r) in &todo {
                rep.evidence.inserted += by_analysis.get(&r.id).map_or(0, |v| v.len());
                rep.card_fts.inserted += src_fts.contains_key(&r.id) as usize;
            }
            rep.evidence.skipped = src_evidence.len() - rep.evidence.inserted;
            rep.card_fts.skipped = src_fts.len() - rep.card_fts.inserted;
        } else {
            rep.evidence.skipped = src_evidence.len();
            rep.card_fts.skipped = src_fts.len();
        }
        info!(
            inserted = rep.analyses.inserted,
            skipped = rep.analyses.skipped,
            "analysis"
        );
    }

    // ---- note ----
    {
        let src: Vec<NoteRow> = fetch_all_map!(from, "SELECT * FROM note", map_note);
        let have = note_keys(to).await?;
        let mut todo: Vec<(i64, &NoteRow)> = Vec::new();
        for r in &src {
            match item_map.get(&r.item_id) {
                None => rep.orphans += 1,
                Some(&tid) if have.contains(&(tid, r.created_at.clone())) => rep.notes.skipped += 1,
                Some(&tid) => todo.push((tid, r)),
            }
        }
        rep.notes.inserted = todo.len();
        if !dry_run && !todo.is_empty() {
            transaction!(to, |tx| {
                for (tid, r) in &todo {
                    sqlx::query("INSERT INTO note (item_id, created_at, body) VALUES (?, ?, ?)")
                        .bind(*tid)
                        .bind(&r.created_at)
                        .bind(&r.body)
                        .execute(&mut *tx)
                        .await?;
                }
            });
        }
        info!(
            inserted = rep.notes.inserted,
            skipped = rep.notes.skipped,
            "note"
        );
    }

    // ---- sync_cursor ----
    {
        let src: Vec<CursorRow> = fetch_all_map!(from, "SELECT * FROM sync_cursor", map_cursor);
        let have = cursor_keys(to).await?;
        let todo: Vec<&CursorRow> = src
            .iter()
            .filter(|r| !have.contains(&(r.source.clone(), r.query_key.clone())))
            .collect();
        rep.cursors.skipped = src.len() - todo.len();
        rep.cursors.inserted = todo.len();
        if !dry_run && !todo.is_empty() {
            transaction!(to, |tx| {
                for r in &todo {
                    bind_all!(
                        sqlx::query(
                            "INSERT INTO sync_cursor (source, query_key, `cursor`, updated_at, \
                             completed_at) VALUES (?, ?, ?, ?, ?)"
                        ),
                        &r.source,
                        &r.query_key,
                        &r.cursor,
                        &r.updated_at,
                        &r.completed_at,
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            });
        }
        info!(
            inserted = rep.cursors.inserted,
            skipped = rep.cursors.skipped,
            "sync_cursor"
        );
    }

    // ---- thesis / thesis_link ----
    {
        let src: Vec<ThesisRow> = fetch_all_map!(from, "SELECT * FROM thesis", map_thesis);
        let mut have = thesis_keys(to).await?;
        let todo: Vec<&ThesisRow> = src
            .iter()
            .filter(|r| !have.contains_key(&(r.title.clone(), r.created_at.clone())))
            .collect();
        rep.theses.skipped = src.len() - todo.len();
        rep.theses.inserted = todo.len();
        if !dry_run && !todo.is_empty() {
            transaction!(to, |tx| {
                for r in &todo {
                    bind_all!(
                        sqlx::query(
                            "INSERT INTO thesis (title, body, status, created_at) \
                             VALUES (?, ?, ?, ?)"
                        ),
                        &r.title,
                        &r.body,
                        &r.status,
                        &r.created_at,
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            });
            have = thesis_keys(to).await?;
        }

        let thesis_map = parent_map(
            &src,
            |r| r.id,
            |r| (r.title.clone(), r.created_at.clone()),
            &have,
            dry_run,
            "thesis",
        )?;

        let links: Vec<ThesisLinkRow> =
            fetch_all_map!(from, "SELECT * FROM thesis_link", map_thesis_link);
        let have_links = thesis_link_keys(to).await?;
        let mut todo: Vec<(i64, i64, &ThesisLinkRow)> = Vec::new();
        for r in &links {
            match (thesis_map.get(&r.thesis_id), item_map.get(&r.item_id)) {
                (Some(&t), Some(&i)) if have_links.contains(&(t, i)) => {
                    rep.thesis_links.skipped += 1
                }
                (Some(&t), Some(&i)) => todo.push((t, i, r)),
                _ => rep.orphans += 1,
            }
        }
        rep.thesis_links.inserted = todo.len();
        if !dry_run && !todo.is_empty() {
            transaction!(to, |tx| {
                for (t, i, r) in &todo {
                    sqlx::query(
                        "INSERT INTO thesis_link (thesis_id, item_id, stance) VALUES (?, ?, ?)",
                    )
                    .bind(*t)
                    .bind(*i)
                    .bind(&r.stance)
                    .execute(&mut *tx)
                    .await?;
                }
            });
        }
    }

    if rep.orphans > 0 {
        warn!(
            rep.orphans,
            "源库里有外键指向不存在的父行，这些行没有搬过去"
        );
    }
    Ok(rep)
}

/// 源库父行 id → 目标库父行 id。
///
/// dry-run 下目标库里还没有那些「会新增」的父行，为了让子表的账也能算准，给它们发一个
/// **负数占位 id**：目标库的真实 id 都是正的自增值，负数不可能撞上，于是子表的存在性检查
/// 必然判为「不存在」—— 正好就是真跑时的结果。
///
/// 真跑时不该出现占位 id（父行刚插完就重读过一遍）。真出现了说明自然键假设不成立，
/// 这时**必须报错而不是继续**：拿一个猜的父 id 去插子表，就是把证据挂到别人的分析上。
fn parent_map<T, K: std::hash::Hash + Eq>(
    rows: &[T],
    id_of: impl Fn(&T) -> i64,
    key_of: impl Fn(&T) -> K,
    existing: &HashMap<K, i64>,
    dry_run: bool,
    table: &str,
) -> Result<HashMap<i64, i64>> {
    let mut map = HashMap::with_capacity(rows.len());
    let mut placeholder = 0i64;
    for r in rows {
        let tid = match existing.get(&key_of(r)) {
            Some(&tid) => tid,
            None if dry_run => {
                placeholder -= 1;
                placeholder
            }
            None => {
                return Err(anyhow!(
                    "{table} 插入后按自然键在目标库里找不回来 —— 停在这里，\n\
                     以免把子表挂到错误的父行上。这说明 {table} 的自然键假设不成立"
                ))
            }
        };
        map.insert(id_of(r), tid);
    }
    Ok(map)
}

// ---------------------------------------------------------------- 目标库已有的键

async fn item_keys(db: &Db) -> Result<HashMap<(String, String), i64>> {
    let rows: Vec<(i64, String, String)> = fetch_all_as!(
        db,
        (i64, String, String),
        "SELECT id, source, source_id FROM item"
    );
    Ok(rows
        .into_iter()
        .map(|(id, s, sid)| ((s, sid), id))
        .collect())
}

async fn comment_keys(db: &Db) -> Result<HashSet<(i64, String)>> {
    let rows: Vec<(i64, String)> = fetch_all_as!(
        db,
        (i64, String),
        "SELECT item_id, source_comment_id FROM comment"
    );
    Ok(rows.into_iter().collect())
}

type AnalysisKey = (i64, String, String, String);

fn analysis_key(target_item_id: i64, r: &AnalysisRow) -> AnalysisKey {
    (
        target_item_id,
        r.created_at.clone(),
        r.prompt_version.clone(),
        r.model.clone(),
    )
}

async fn analysis_keys(db: &Db) -> Result<HashSet<AnalysisKey>> {
    let rows: Vec<AnalysisKey> = fetch_all_as!(
        db,
        AnalysisKey,
        "SELECT item_id, created_at, prompt_version, model FROM analysis"
    );
    Ok(rows.into_iter().collect())
}

async fn note_keys(db: &Db) -> Result<HashSet<(i64, String)>> {
    let rows: Vec<(i64, String)> =
        fetch_all_as!(db, (i64, String), "SELECT item_id, created_at FROM note");
    Ok(rows.into_iter().collect())
}

async fn cursor_keys(db: &Db) -> Result<HashSet<(String, String)>> {
    let rows: Vec<(String, String)> = fetch_all_as!(
        db,
        (String, String),
        "SELECT source, query_key FROM sync_cursor"
    );
    Ok(rows.into_iter().collect())
}

async fn thesis_keys(db: &Db) -> Result<HashMap<(String, String), i64>> {
    let rows: Vec<(i64, String, String)> = fetch_all_as!(
        db,
        (i64, String, String),
        "SELECT id, title, created_at FROM thesis"
    );
    Ok(rows.into_iter().map(|(id, t, c)| ((t, c), id)).collect())
}

async fn thesis_link_keys(db: &Db) -> Result<HashSet<(i64, i64)>> {
    let rows: Vec<(i64, i64)> =
        fetch_all_as!(db, (i64, i64), "SELECT thesis_id, item_id FROM thesis_link");
    Ok(rows.into_iter().collect())
}

/// 搬之前的一道防呆：两端指向同一个库就直接拒绝。
pub fn reject_same_endpoint(from: &Db, to: &Db) -> Result<()> {
    if from.backend() == to.backend() {
        return Err(anyhow!(
            "源和目标是同一种后端（{}）—— 这个命令只在 SQLite 和 MySQL 之间搬",
            from.backend().as_str()
        ));
    }
    Ok(())
}
