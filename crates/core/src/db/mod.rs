//! 仓储层。**SQLite（默认）和 MySQL 两个后端**，由 `[db] backend` 选。
//!
//! 刻意用运行期的 `sqlx::query`（而不是 `query!` 宏）：宏要求编译期能连上一个真实
//! 数据库，会让 `cargo check` 依赖 `DATABASE_URL`。对一个自用工具来说不值这个麻烦 ——
//! 而且两个后端的方言不同，编译期校验本来也只能校验其中一个。
//!
//! # 两个后端怎么共用一份 SQL
//!
//! `sqlx` 的类型是按驱动分开的（`Query<'_, Sqlite, _>` / `Query<'_, MySql, _>`），
//! 没法用一个变量装下两种。所以这里的做法是：
//!
//! - **语句文本只写一遍**，占位符统一用 `?`（两个后端都接受；SQLite 的 `?1`
//!   编号写法 MySQL 不认，所以全改成了裸 `?`，绑定顺序即参数顺序）
//! - 方言差异集中在 [`Db`] 的几个小方法里（upsert 子句、取自增 id、COUNT 的类型）
//! - 分发用宏做（[`execute!`] / [`fetch_all_map!`] …）：宏为每个后端各展开一份，
//!   调用处只写一次
//! - 行映射用宏生成的泛型函数（`row_mapper!`），函数体同样只写一次
//!
//! 这比「一个 trait 两套实现」少了 400 行重复 SQL，代价是宏。**新增查询时照抄
//! 现有形状即可**，不要在业务代码里直接 match 后端。

use anyhow::{Context, Result};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{ColumnIndex, Decode, Row, Type};
use std::path::Path;
use std::str::FromStr;
use tracing::info;

use crate::config::{DbBackend, DbConfig};
use crate::model::{
    Analysis, Comment, Evidence, Item, ItemSummary, NewComment, NewItem, OpportunityCard, Tri,
    Usage, Verdict,
};

// ---------------------------------------------------------------- 分发宏

/// 把绑定参数依次塞进一个 query。两个后端各展开一份，所以绑定列表只写一次。
macro_rules! bind_all {
    ($q:expr $(, $b:expr)* $(,)?) => {{
        let q = $q;
        $( let q = q.bind($b); )*
        q
    }};
}

/// 执行一条不关心返回行的语句。
macro_rules! execute {
    ($db:expr, $sql:expr $(, $b:expr)* $(,)?) => {{
        match $db {
            Db::Sqlite(p) => { bind_all!(sqlx::query($sql) $(, $b)*).execute(p).await?; }
            Db::MySql(p)  => { bind_all!(sqlx::query($sql) $(, $b)*).execute(p).await?; }
        }
    }};
}

/// 取全部行，用一个泛型映射函数转成领域类型。
macro_rules! fetch_all_map {
    ($db:expr, $sql:expr, $map:expr $(, $b:expr)* $(,)?) => {{
        match $db {
            Db::Sqlite(p) => bind_all!(sqlx::query($sql) $(, $b)*)
                .fetch_all(p).await?.iter().map($map).collect::<Result<Vec<_>>>()?,
            Db::MySql(p) => bind_all!(sqlx::query($sql) $(, $b)*)
                .fetch_all(p).await?.iter().map($map).collect::<Result<Vec<_>>>()?,
        }
    }};
}

/// 取零或一行，同上。
macro_rules! fetch_optional_map {
    ($db:expr, $sql:expr, $map:expr $(, $b:expr)* $(,)?) => {{
        match $db {
            Db::Sqlite(p) => match bind_all!(sqlx::query($sql) $(, $b)*).fetch_optional(p).await? {
                Some(row) => Some($map(&row)?),
                None => None,
            },
            Db::MySql(p) => match bind_all!(sqlx::query($sql) $(, $b)*).fetch_optional(p).await? {
                Some(row) => Some($map(&row)?),
                None => None,
            },
        }
    }};
}

/// `query_as` 版本，用来取标量 / 元组。
macro_rules! fetch_all_as {
    ($db:expr, $ty:ty, $sql:expr $(, $b:expr)* $(,)?) => {{
        match $db {
            Db::Sqlite(p) => bind_all!(sqlx::query_as::<_, $ty>($sql) $(, $b)*).fetch_all(p).await?,
            Db::MySql(p)  => bind_all!(sqlx::query_as::<_, $ty>($sql) $(, $b)*).fetch_all(p).await?,
        }
    }};
}

macro_rules! fetch_optional_as {
    ($db:expr, $ty:ty, $sql:expr $(, $b:expr)* $(,)?) => {{
        match $db {
            Db::Sqlite(p) => bind_all!(sqlx::query_as::<_, $ty>($sql) $(, $b)*).fetch_optional(p).await?,
            Db::MySql(p)  => bind_all!(sqlx::query_as::<_, $ty>($sql) $(, $b)*).fetch_optional(p).await?,
        }
    }};
}

/// 在一个事务里跑 `$body`，正常结束即提交。
///
/// 除了原本就该原子的多表写入（analysis + evidence），**所有需要拿自增 id 的插入
/// 也都走这里** —— 见 [`Db::last_id_sql`]：取 id 是第二条语句，必须和 INSERT 落在
/// 同一条连接上，而连接池不保证这一点。
macro_rules! transaction {
    ($db:expr, |$tx:ident| $body:block) => {{
        match $db {
            Db::Sqlite(p) => {
                let mut $tx = p.begin().await?;
                let out = $body;
                $tx.commit().await?;
                out
            }
            Db::MySql(p) => {
                let mut $tx = p.begin().await?;
                let out = $body;
                $tx.commit().await?;
                out
            }
        }
    }};
}

// ---------------------------------------------------------------- 连接

/// 连接池。`Clone` 很便宜（内部是 Arc）。
#[derive(Clone)]
pub enum Db {
    Sqlite(SqlitePool),
    MySql(MySqlPool),
}

impl Db {
    pub fn backend(&self) -> DbBackend {
        match self {
            Db::Sqlite(_) => DbBackend::Sqlite,
            Db::MySql(_) => DbBackend::Mysql,
        }
    }

    /// upsert 的冲突子句。`keys` 是唯一键列，`cols` 是冲突时要覆盖的列。
    ///
    /// SQLite 用 `excluded.x`，MySQL 用 `VALUES(x)`（8.0.20 起标记 deprecated，
    /// 但换成 `AS new` 别名写法就丢掉 5.7 和 MariaDB —— 自用工具里兼容性更值钱）。
    fn upsert_clause(&self, keys: &str, cols: &[&str]) -> String {
        let sets: Vec<String> = match self {
            Db::Sqlite(_) => cols.iter().map(|c| format!("{c} = excluded.{c}")).collect(),
            Db::MySql(_) => cols.iter().map(|c| format!("{c} = VALUES({c})")).collect(),
        };
        let sets = sets.join(",\n            ");
        match self {
            Db::Sqlite(_) => format!("ON CONFLICT ({keys}) DO UPDATE SET {sets}"),
            Db::MySql(_) => format!("ON DUPLICATE KEY UPDATE {sets}"),
        }
    }

    /// 刚插入那行的自增 id。**只在同一条连接（事务）里紧跟 INSERT 用**。
    ///
    /// 不用 `RETURNING id` 是因为 MySQL 不支持；不用 `last_insert_id()` 的
    /// 驱动侧返回值是因为那样就得在事务体里按后端分叉，宏做不到。
    fn last_id_sql(&self) -> &'static str {
        match self {
            Db::Sqlite(_) => "SELECT last_insert_rowid() AS id",
            // MySQL 的 LAST_INSERT_ID() 是 BIGINT UNSIGNED，sqlx 不肯解成 i64
            Db::MySql(_) => "SELECT CAST(LAST_INSERT_ID() AS SIGNED) AS id",
        }
    }

    /// `COUNT(...)` 表达式。同理：MySQL 的 COUNT 是无符号的，得套 CAST。
    fn count(&self, inner: &str) -> String {
        match self {
            Db::Sqlite(_) => format!("COUNT({inner})"),
            Db::MySql(_) => format!("CAST(COUNT({inner}) AS SIGNED)"),
        }
    }
}

/// 按配置连上数据库。默认 SQLite。
pub async fn connect(cfg: &DbConfig) -> Result<Db> {
    match cfg.backend {
        DbBackend::Sqlite => connect_sqlite(&cfg.path, cfg.max_connections).await,
        DbBackend::Mysql => connect_mysql(&cfg.resolve_mysql_url()?, cfg.max_connections).await,
    }
}

/// 单文件 SQLite。文件不存在就建，父目录不存在也一并建。
pub async fn connect_sqlite(path: &Path, max_connections: u32) -> Result<Db> {
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
        .max_connections(max_connections.max(1))
        .connect_with(opts)
        .await
        .with_context(|| format!("打开数据库失败: {}", path.display()))?;

    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(&pool)
        .await?;
    Ok(Db::Sqlite(pool))
}

/// MySQL。库不存在会尝试建（和 SQLite 的 `create_if_missing` 对齐）；
/// 没有 CREATE 权限时不当致命错误 —— 让后面的连接失败给出真正的原因。
pub async fn connect_mysql(url: &str, max_connections: u32) -> Result<Db> {
    use sqlx::migrate::MigrateDatabase;

    match sqlx::MySql::database_exists(url).await {
        Ok(true) => {}
        Ok(false) => match sqlx::MySql::create_database(url).await {
            Ok(()) => info!("目标数据库不存在，已创建"),
            Err(e) => {
                tracing::warn!(error = %e, "目标数据库不存在且自动创建失败，请手动 CREATE DATABASE")
            }
        },
        Err(e) => tracing::debug!(error = %e, "跳过数据库存在性检查"),
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(max_connections.max(1))
        .connect(url)
        .await
        .with_context(|| {
            format!(
                "连接 MySQL 失败: {}。检查主机 / 端口 / 账号密码，以及库是否已创建",
                redact(url)
            )
        })?;
    Ok(Db::MySql(pool))
}

/// 把 DSN 里的密码抹掉再进日志和错误信息。
fn redact(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(scheme), Some(at)) if at > scheme + 3 => {
            let creds = &url[scheme + 3..at];
            let user = creds.split(':').next().unwrap_or("");
            format!("{}://{}:***@{}", &url[..scheme], user, &url[at + 1..])
        }
        _ => url.to_string(),
    }
}

/// 跑迁移。两个后端各有一套 SQL（`migrations/{sqlite,mysql}/`），
/// **版本号一一对应** —— 加迁移时两边都要加，否则换后端会拿到不同的 schema。
pub async fn migrate(db: &Db) -> Result<()> {
    let r = match db {
        Db::Sqlite(p) => sqlx::migrate!("../../migrations/sqlite").run(p).await,
        Db::MySql(p) => sqlx::migrate!("../../migrations/mysql").run(p).await,
    };
    // MySQL 的 DDL 不在事务里：一条迁移中途失败，库就停在半成品状态，而 sqlx 给的提示
    // （"fix and remove row from _sqlx_migrations"）没说怎么修。这里把修法直接写出来。
    r.with_context(|| match db {
        Db::Sqlite(_) => "执行 migrations 失败".to_string(),
        Db::MySql(_) => "执行 migrations 失败。\n\
             MySQL / MariaDB 的 DDL 不在事务里 —— 失败的迁移不会回滚，库会停在半成品状态\n\
             （部分表已建，_sqlx_migrations 里留一条 success=false 的记录），之后每次启动都报\n\
             「partially applied」。\n\
             \n\
             先查状态（会指出是哪种原因）：\n\
               PHI_TEST_MYSQL_URL=$PHI_DB_URL cargo test -p phi-core --test mysql_admin -- --nocapture\n\
             \n\
             最常见的原因是**改了 migrations/ 下的文件但没重新编译** —— 迁移 SQL 是被\n\
             sqlx::migrate! 编译进二进制的，先 cargo build 再试。\n\
             确实要重来就清空该库：PHI_ADMIN_ACTION=drop_all（同一条命令）。"
            .to_string(),
    })?;
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------- 行映射

/// 定义一个行映射函数：对 `SqliteRow` 和 `MySqlRow` 各 monomorphize 一份，
/// 函数体只写一遍。
///
/// 存在的唯一理由是那串 `where`：按列名取值需要 `ColumnIndex`，每种取出来的
/// Rust 类型又各需要一对 `Decode` / `Type`。这些约束**不能**收进一个 trait ——
/// 写在 trait 定义上的 where 只约束实现者，不会作为隐含约束传给使用 `R: Trait`
/// 的泛型函数（rustc 会要求你在每个函数上再抄一遍）。所以用宏抄。
macro_rules! row_mapper {
    (fn $name:ident($row:ident) -> $ty:ty $body:block) => {
        fn $name<R>($row: &R) -> Result<$ty>
        where
            R: Row,
            for<'a> &'a str: ColumnIndex<R>,
            for<'a> i64: Decode<'a, R::Database> + Type<R::Database>,
            for<'a> f64: Decode<'a, R::Database> + Type<R::Database>,
            for<'a> String: Decode<'a, R::Database> + Type<R::Database>,
        $body
    };
}

row_mapper!(fn row_to_item(row) -> Item {
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
});

row_mapper!(fn row_to_comment(row) -> Comment {
    Ok(Comment {
        id: row.try_get("id")?,
        item_id: row.try_get("item_id")?,
        source_comment_id: row.try_get("source_comment_id")?,
        author: row.try_get("author")?,
        // 布尔一律按 i64 存取：MySQL 侧是 BIGINT，sqlx 不做窄化解码
        is_maker: row.try_get::<i64, _>("is_maker")? != 0,
        body: row.try_get("body")?,
        votes: row.try_get("votes")?,
        created_at: row.try_get("created_at")?,
        kept: row.try_get::<i64, _>("kept")? != 0,
        filter_reason: row.try_get("filter_reason")?,
    })
});

row_mapper!(fn row_to_analysis(row) -> Analysis {
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
});

row_mapper!(fn row_to_summary(r) -> ItemSummary {
    let tri = |col: &str| -> Result<Option<Tri>> {
        Ok(r.try_get::<Option<String>, _>(col)?
            .as_deref()
            .and_then(Tri::parse))
    };
    Ok(ItemSummary {
        item: row_to_item(r)?,
        latest_analysis_id: r.try_get("a_id")?,
        verdict: r
            .try_get::<Option<String>, _>("a_verdict")?
            .as_deref()
            .and_then(Verdict::parse),
        buildable: tri("a_buildable")?,
        worth_it: tri("a_worth_it")?,
        reachable: tri("a_reachable")?,
        analysis_count: r.try_get("analysis_count")?,
        note_count: r.try_get("note_count")?,
    })
});

// SQLite ↔ MySQL 的数据搬运。放在 db 下面是为了复用上面那几个分发宏和 `row_mapper!`
// （`macro_rules!` 是文本作用域：子模块必须声明在宏定义之后）。
pub mod transfer;

// ---------------------------------------------------------------- item

/// 插入或更新一条 item，返回其 id。
pub async fn upsert_item(db: &Db, it: &NewItem) -> Result<i64> {
    Ok(upsert_item_tracked(db, it).await?.0)
}

/// 同 [`upsert_item`]，额外返回这次是不是新插入的行。sync 用它区分「新增」和「更新」。
pub async fn upsert_item_tracked(db: &Db, it: &NewItem) -> Result<(i64, bool)> {
    let existing: Option<(i64,)> = fetch_optional_as!(
        db,
        (i64,),
        "SELECT id FROM item WHERE source = ? AND source_id = ?",
        &it.source,
        &it.source_id,
    );

    let topics = serde_json::to_string(&it.topics)?;
    let raw = serde_json::to_string(&it.raw)?;
    let sql = format!(
        r#"
        INSERT INTO item (source, source_id, slug, url, name, tagline, description,
                          website, posted_at, signal_count, vote_count, topics, raw, fetched_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        {}
        "#,
        db.upsert_clause(
            "source, source_id",
            &[
                "slug",
                "url",
                "name",
                "tagline",
                "description",
                "website",
                "posted_at",
                "signal_count",
                "vote_count",
                "topics",
                "raw",
                "fetched_at",
            ],
        )
    );

    let id = transaction!(db, |tx| {
        bind_all!(
            sqlx::query(&sql),
            &it.source,
            &it.source_id,
            &it.slug,
            &it.url,
            &it.name,
            &it.tagline,
            &it.description,
            &it.website,
            &it.posted_at,
            it.signal_count,
            it.vote_count,
            &topics,
            &raw,
            now(),
        )
        .execute(&mut *tx)
        .await?;

        // 走了更新分支时自增 id 没动过，不能问数据库要 —— 用先前查到的那个
        match existing {
            Some((id,)) => id,
            None => {
                sqlx::query_as::<_, (i64,)>(db.last_id_sql())
                    .fetch_one(&mut *tx)
                    .await?
                    .0
            }
        }
    });

    Ok((id, existing.is_none()))
}

pub async fn get_item(db: &Db, id: i64) -> Result<Option<Item>> {
    Ok(fetch_optional_map!(
        db,
        "SELECT * FROM item WHERE id = ?",
        row_to_item,
        id
    ))
}

/// 列出条目。`min_signal` 就是「评论够多才值得深挖」那道可调阈值 ——
/// 它是**视图层的过滤**，不是采集时的硬筛：阈值选错了不用重拉数据。
pub async fn list_items(
    db: &Db,
    min_signal: i64,
    limit: i64,
    analyzed: Option<bool>,
) -> Result<Vec<Item>> {
    let sql = match analyzed {
        None => {
            "SELECT * FROM item WHERE signal_count >= ? \
             ORDER BY signal_count DESC LIMIT ?"
        }
        Some(true) => {
            "SELECT i.* FROM item i WHERE i.signal_count >= ? \
             AND EXISTS (SELECT 1 FROM analysis a WHERE a.item_id = i.id) \
             ORDER BY i.signal_count DESC LIMIT ?"
        }
        Some(false) => {
            "SELECT i.* FROM item i WHERE i.signal_count >= ? \
             AND NOT EXISTS (SELECT 1 FROM analysis a WHERE a.item_id = i.id) \
             ORDER BY i.signal_count DESC LIMIT ?"
        }
    };
    Ok(fetch_all_map!(db, sql, row_to_item, min_signal, limit))
}

pub async fn mark_comments_fetched(db: &Db, item_id: i64) -> Result<()> {
    execute!(
        db,
        "UPDATE item SET comments_fetched_at = ? WHERE id = ?",
        now(),
        item_id
    );
    Ok(())
}

/// 某个源的全部 item。给按源清理 / 按源统计用 —— `list_items` 带 limit 且按热度排序，
/// 库大了之后拿它做「全量扫某个源」会静默漏数据。
pub async fn items_by_source(db: &Db, source: &str) -> Result<Vec<Item>> {
    Ok(fetch_all_map!(
        db,
        "SELECT * FROM item WHERE source = ? ORDER BY id",
        row_to_item,
        source
    ))
}

/// item 总数。
pub async fn count_items(db: &Db) -> Result<i64> {
    let sql = format!("SELECT {} FROM item", db.count("*"));
    let (n,): (i64,) = fetch_all_as!(db, (i64,), &sql).remove(0);
    Ok(n)
}

// ---------------------------------------------------------------- comment

pub async fn upsert_comments(
    db: &Db,
    item_id: i64,
    comments: &[NewComment],
    decisions: &[crate::filter::Decision],
) -> Result<()> {
    let sql = format!(
        r#"
        INSERT INTO comment (item_id, source_comment_id, author, is_maker, body,
                             votes, created_at, kept, filter_reason)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        {}
        "#,
        db.upsert_clause(
            "item_id, source_comment_id",
            &["body", "votes", "is_maker", "kept", "filter_reason"],
        )
    );

    transaction!(db, |tx| {
        for (c, d) in comments.iter().zip(decisions) {
            bind_all!(
                sqlx::query(&sql),
                item_id,
                &c.source_comment_id,
                &c.author,
                c.is_maker as i64,
                &c.body,
                c.votes,
                &c.created_at,
                d.kept as i64,
                &d.reason,
            )
            .execute(&mut *tx)
            .await?;
        }
    });
    Ok(())
}

pub async fn load_comments(db: &Db, item_id: i64, kept_only: bool) -> Result<Vec<Comment>> {
    let sql = if kept_only {
        "SELECT * FROM comment WHERE item_id = ? AND kept = 1 ORDER BY votes DESC"
    } else {
        "SELECT * FROM comment WHERE item_id = ? ORDER BY votes DESC"
    };
    Ok(fetch_all_map!(db, sql, row_to_comment, item_id))
}

/// 某条 item 下的评论条数。
pub async fn count_comments(db: &Db, item_id: i64) -> Result<i64> {
    let sql = format!("SELECT {} FROM comment WHERE item_id = ?", db.count("*"));
    let (n,): (i64,) = fetch_all_as!(db, (i64,), &sql, item_id).remove(0);
    Ok(n)
}

// ---------------------------------------------------------------- analysis

/// 写入一次分析。**追加，不覆盖** —— 旧版本保留才能做 diff。
#[allow(clippy::too_many_arguments)]
pub async fn insert_analysis(
    db: &Db,
    item_id: i64,
    prompt_version: &str,
    model: &str,
    input_snapshot: &serde_json::Value,
    card: &OpportunityCard,
    usage: &Usage,
) -> Result<i64> {
    let card_json = serde_json::to_string(card)?;
    let snapshot_json = serde_json::to_string(input_snapshot)?;

    let push = |field: &str, ev: &[Evidence]| -> Vec<(String, String, String)> {
        ev.iter()
            .map(|e| (field.to_string(), e.quote.clone(), e.source_ref.clone()))
            .collect()
    };
    let mut evidence = push("pain", &card.pain_evidence);
    evidence.extend(push("who_pays", &card.who_pays_evidence));
    evidence.extend(push("gap", &card.gap_evidence));
    let search_text = card.to_search_text();

    let analysis_id = transaction!(db, |tx| {
        bind_all!(
            sqlx::query(
                r#"
                INSERT INTO analysis (item_id, created_at, prompt_version, model, provider,
                                      input_snapshot, card, buildable, worth_it, reachable,
                                      verdict, tokens_in, tokens_out, cost_usd)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#
            ),
            item_id,
            now(),
            prompt_version,
            model,
            &usage.provider,
            &snapshot_json,
            &card_json,
            card.buildable.value.as_str(),
            card.worth_it.value.as_str(),
            card.reachable.value.as_str(),
            card.verdict.as_str(),
            usage.tokens_in,
            usage.tokens_out,
            usage.cost_usd,
        )
        .execute(&mut *tx)
        .await?;

        let analysis_id: i64 = sqlx::query_as::<_, (i64,)>(db.last_id_sql())
            .fetch_one(&mut *tx)
            .await?
            .0;

        for (field, quote, source_ref) in &evidence {
            sqlx::query(
                "INSERT INTO evidence (analysis_id, field, quote, source_ref) VALUES (?, ?, ?, ?)",
            )
            .bind(analysis_id)
            .bind(field)
            .bind(quote)
            .bind(source_ref)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query("INSERT INTO card_fts (rowid, card_text) VALUES (?, ?)")
            .bind(analysis_id)
            .bind(&search_text)
            .execute(&mut *tx)
            .await?;

        analysis_id
    });

    Ok(analysis_id)
}

pub async fn get_analysis(db: &Db, id: i64) -> Result<Option<Analysis>> {
    Ok(fetch_optional_map!(
        db,
        "SELECT * FROM analysis WHERE id = ?",
        row_to_analysis,
        id
    ))
}

pub async fn latest_analysis(db: &Db, item_id: i64) -> Result<Option<Analysis>> {
    Ok(fetch_optional_map!(
        db,
        "SELECT * FROM analysis WHERE item_id = ? ORDER BY created_at DESC, id DESC LIMIT 1",
        row_to_analysis,
        item_id
    ))
}

/// 取某条 item 的全部分析版本，新的在前。换 prompt / 换模型重跑后用它做对比。
pub async fn analysis_history(db: &Db, item_id: i64) -> Result<Vec<Analysis>> {
    Ok(fetch_all_map!(
        db,
        "SELECT * FROM analysis WHERE item_id = ? ORDER BY created_at DESC, id DESC",
        row_to_analysis,
        item_id
    ))
}

/// 当初实际喂进模型的输入。reanalyze 靠它保证只有 prompt / model 在变。
pub async fn get_input_snapshot(db: &Db, analysis_id: i64) -> Result<Option<String>> {
    let row: Option<(String,)> = fetch_optional_as!(
        db,
        (String,),
        "SELECT input_snapshot FROM analysis WHERE id = ?",
        analysis_id
    );
    Ok(row.map(|r| r.0))
}

/// 某次分析拆出来的证据条目，按字段排序。
pub async fn evidence_for(db: &Db, analysis_id: i64) -> Result<Vec<(String, String)>> {
    Ok(fetch_all_as!(
        db,
        (String, String),
        "SELECT field, quote FROM evidence WHERE analysis_id = ? ORDER BY field, id",
        analysis_id
    ))
}

/// 删掉一次分析。evidence 随外键级联；`card_fts` 没有外键，得手动删。
/// 笔记不受影响 —— 两条线完全分离。
pub async fn delete_analysis(db: &Db, analysis_id: i64) -> Result<()> {
    transaction!(db, |tx| {
        sqlx::query("DELETE FROM card_fts WHERE rowid = ?")
            .bind(analysis_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM analysis WHERE id = ?")
            .bind(analysis_id)
            .execute(&mut *tx)
            .await?;
    });
    Ok(())
}

/// 删掉一条 item 及其全部评论 / 分析 / 证据 / 笔记（外键级联），
/// 外加那些分析在 `card_fts` 里的行 —— 那张表没有外键。
pub async fn delete_item(db: &Db, item_id: i64) -> Result<()> {
    transaction!(db, |tx| {
        sqlx::query(
            "DELETE FROM card_fts WHERE rowid IN (SELECT id FROM analysis WHERE item_id = ?)",
        )
        .bind(item_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM item WHERE id = ?")
            .bind(item_id)
            .execute(&mut *tx)
            .await?;
    });
    Ok(())
}

// ---------------------------------------------------------------- note

/// 你的笔记。独立一条线 —— 分析可以删了重来，笔记不受影响。
pub async fn insert_note(db: &Db, item_id: i64, body: &str) -> Result<i64> {
    let id = transaction!(db, |tx| {
        sqlx::query("INSERT INTO note (item_id, created_at, body) VALUES (?, ?, ?)")
            .bind(item_id)
            .bind(now())
            .bind(body)
            .execute(&mut *tx)
            .await?;
        sqlx::query_as::<_, (i64,)>(db.last_id_sql())
            .fetch_one(&mut *tx)
            .await?
            .0
    });
    Ok(id)
}

pub async fn load_notes(db: &Db, item_id: i64) -> Result<Vec<(String, String)>> {
    Ok(fetch_all_as!(
        db,
        (String, String),
        "SELECT created_at, body FROM note WHERE item_id = ? ORDER BY created_at DESC",
        item_id
    ))
}

// ---------------------------------------------------------------- sync 游标

#[derive(Debug, Clone, Default)]
pub struct SyncCursor {
    pub cursor: Option<String>,
    pub completed_at: Option<String>,
}

pub async fn get_sync_cursor(db: &Db, source: &str, key: &str) -> Result<SyncCursor> {
    let row: Option<(Option<String>, Option<String>)> = fetch_optional_as!(
        db,
        (Option<String>, Option<String>),
        "SELECT `cursor`, completed_at FROM sync_cursor WHERE source = ? AND query_key = ?",
        source,
        key
    );
    Ok(row
        .map(|(cursor, completed_at)| SyncCursor {
            cursor,
            completed_at,
        })
        .unwrap_or_default())
}

/// 记下翻到哪了。`completed = true` 表示这个切片已经翻完。
pub async fn save_sync_cursor(
    db: &Db,
    source: &str,
    key: &str,
    cursor: Option<&str>,
    completed: bool,
) -> Result<()> {
    let at = now();
    let sql = format!(
        r#"
        INSERT INTO sync_cursor (source, query_key, `cursor`, updated_at, completed_at)
        VALUES (?, ?, ?, ?, ?)
        {}
        "#,
        db.upsert_clause(
            "source, query_key",
            // CURSOR 是 MySQL / MariaDB 的保留字。反引号两个后端都认
            &["`cursor`", "updated_at", "completed_at"],
        )
    );
    execute!(db, &sql, source, key, cursor, &at, completed.then_some(&at));
    Ok(())
}

/// 清掉某个源的全部 sync 游标。想让一个源从头重拉时用它。
pub async fn delete_sync_cursors(db: &Db, source: &str) -> Result<()> {
    execute!(db, "DELETE FROM sync_cursor WHERE source = ?", source);
    Ok(())
}

// ---------------------------------------------------------------- 浏览视图

/// 全部 item，各自带上最新一次分析的摘要列（analysis 表里冗余存的那几列，不用解析卡片 JSON）。
pub async fn list_summaries(db: &Db) -> Result<Vec<ItemSummary>> {
    let sql = format!(
        r#"
        SELECT i.*,
               a.id        AS a_id,
               a.verdict   AS a_verdict,
               a.buildable AS a_buildable,
               a.worth_it  AS a_worth_it,
               a.reachable AS a_reachable,
               (SELECT {count} FROM analysis x WHERE x.item_id = i.id) AS analysis_count,
               (SELECT {count} FROM note n WHERE n.item_id = i.id)     AS note_count
        FROM item i
        LEFT JOIN analysis a ON a.id = (
            SELECT id FROM analysis WHERE item_id = i.id ORDER BY created_at DESC, id DESC LIMIT 1
        )
        "#,
        count = db.count("*")
    );
    Ok(fetch_all_map!(db, &sql, row_to_summary))
}

/// LIKE 用的转义。转义字符选 `!` 而不是反斜杠：MySQL 的字符串字面量自己会吃掉
/// 反斜杠，`ESCAPE '\'` 在那边根本解析不过去，而 `'\\'` 在 SQLite 那边又是两个字符。
fn like_pattern(needle: &str) -> String {
    let escaped = needle
        .replace('!', "!!")
        .replace('%', "!%")
        .replace('_', "!_");
    format!("%{escaped}%")
}

/// 子串匹配的 item id：名称 / tagline / 描述 / 任一版本卡片正文 / 笔记。
///
/// 刻意不用 FTS5 的 MATCH：SQLite 侧三张 fts 表用的是默认 `unicode61` 分词器，
/// 它不切中文 —— 一整句中文会被当成一个 token，搜「律所」根本命中不了卡片正文。
/// 自用工具的数据量下 LIKE 全表扫描毫无压力，而且这样两个后端能共用同一条 SQL。
pub async fn item_ids_containing(db: &Db, needle: &str) -> Result<Vec<i64>> {
    let p = like_pattern(needle);
    // 裸 `?` 是按顺序绑定的，同一个 pattern 用到几次就绑几次
    let rows: Vec<(i64,)> = fetch_all_as!(
        db,
        (i64,),
        r#"
        SELECT id FROM item
         WHERE name LIKE ? ESCAPE '!' OR tagline LIKE ? ESCAPE '!' OR description LIKE ? ESCAPE '!'
        UNION
        SELECT a.item_id FROM analysis a JOIN card_fts c ON c.rowid = a.id
         WHERE c.card_text LIKE ? ESCAPE '!'
        UNION
        SELECT item_id FROM note WHERE body LIKE ? ESCAPE '!'
        "#,
        &p,
        &p,
        &p,
        &p,
        &p
    );
    Ok(rows.into_iter().map(|r| r.0).collect())
}

// ---------------------------------------------------------------- search

/// `phi search`。
///
/// 两个后端的语义不同，这是刻意的：
/// - SQLite 走 FTS5，`q` 是 FTS5 查询语法（支持 `AND` / 前缀 `*` 等）
/// - MySQL 没有 fts 镜像表（见 `migrations/mysql/0001_init.sql` 末尾的说明），
///   走子串 LIKE，按讨论热度排序。中文查询下这反而比分词更准。
pub async fn search_items(db: &Db, q: &str, limit: i64) -> Result<Vec<Item>> {
    match db {
        Db::Sqlite(_) => Ok(fetch_all_map!(
            db,
            "SELECT i.* FROM item i JOIN item_fts f ON f.rowid = i.id \
             WHERE item_fts MATCH ? ORDER BY rank LIMIT ?",
            row_to_item,
            q,
            limit
        )),
        Db::MySql(_) => {
            let p = like_pattern(q);
            Ok(fetch_all_map!(
                db,
                "SELECT * FROM item \
                 WHERE name LIKE ? ESCAPE '!' OR tagline LIKE ? ESCAPE '!' \
                    OR description LIKE ? ESCAPE '!' \
                 ORDER BY signal_count DESC, id DESC LIMIT ?",
                row_to_item,
                &p,
                &p,
                &p,
                limit
            ))
        }
    }
}
