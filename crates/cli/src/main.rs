//! `phi` —— ProductHunt inspiration tool.
//!
//! 所有子命令都是 `phi_core::usecase` 里函数的薄包装。业务流程不写在这里。

use anyhow::{anyhow, Context, Result};
use chrono::{NaiveDate, Utc};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use phi_analyzer::OpenRouterAnalyzer;
use phi_core::model::SyncRequest;
use phi_core::{config::Config, db, render, usecase, Ctx};
use phi_sources::ProductHunt;

mod tui;

#[derive(Parser)]
#[command(
    name = "phi",
    about = "ProductHunt inspiration tool —— 把有真实讨论的产品拆成带证据的机会卡片",
    version
)]
struct Cli {
    /// 配置文件路径，默认读当前目录的 config.toml
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// 覆盖 analyzer.model。验收门没过时用它做归因：
    /// 同一批快照换个更强的模型重跑，区分是 prompt 烂还是模型弱。
    #[arg(long, global = true)]
    model: Option<String>,

    #[arg(long, global = true, default_value = "info")]
    log: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 主循环：抓取 → 评论 → 过滤 → 分析 → 打印卡片
    Add {
        /// ProductHunt 产品页 URL
        url: String,
    },

    /// 阶段一：按天批量拉轻量元数据入库（不碰评论、不做分析）。中断后重跑会续传
    Sync {
        #[arg(long)]
        topic: Option<String>,
        /// 起始日期（UTC），形如 2026-09-01。默认今天
        #[arg(long)]
        since: Option<String>,
        /// 结束日期（UTC，含当天）。默认今天
        #[arg(long)]
        until: Option<String>,
        /// 本次最多发多少个请求（每个 20 条、扣 100 点配额）。用完就停，下次续传。默认不限
        #[arg(long)]
        max_pages: Option<usize>,
        /// 忽略已完成标记，从头重拉（用来刷新旧日期的票数和评论数）
        #[arg(long)]
        refresh: bool,
    },

    /// 阶段二：对通过阈值的候选拉评论
    Hydrate {
        #[arg(long, default_value_t = 15)]
        min_comments: i64,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },

    /// 分析一条已入库的 item
    Analyze { item_id: i64 },

    /// 用同一份输入快照重跑分析（不重新抓取，保证只有 prompt/model 在变）
    Reanalyze {
        analysis_id: i64,
        /// 同时和原版本并排对比
        #[arg(long)]
        diff: bool,
    },

    /// 并排对比两次分析
    Diff { a: i64, b: i64 },

    /// 列出条目。--min-comments 就是那道可调阈值
    Ls {
        #[arg(long, default_value_t = 0)]
        min_comments: i64,
        #[arg(long, default_value_t = 30)]
        limit: i64,
        /// 只看还没分析过的
        #[arg(long)]
        pending: bool,
    },

    /// 查看某条 item 最新的卡片
    Show { item_id: i64 },

    /// 写一条笔记。笔记和 AI 分析是两条完全独立的线，互不覆盖。
    Note {
        item_id: i64,
        /// 直接给正文；不给则从 stdin 读
        body: Option<String>,
    },

    /// 全文检索
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },

    /// 看某条 item 的评论过滤结果。跑完前 20 个产品后用它抽查误杀率。
    Filtered { item_id: i64 },

    /// 在 SQLite 和 MySQL 之间搬数据：只增，不改，不删。幂等，可反复跑
    Dbsync {
        /// 反向：MySQL → SQLite。默认是 SQLite → MySQL
        #[arg(long)]
        reverse: bool,
        /// 只报告会插入多少行，一行都不写
        #[arg(long)]
        dry_run: bool,
    },

    /// 打印发给模型的 JSON Schema（调试用）
    Schema,

    /// 三栏浏览：左列表（可筛可排）、中卡片、右笔记。按 ? 看按键
    Tui {
        /// 评论数阈值的初始值，进去之后用 +/- 调
        #[arg(long, default_value_t = 0)]
        min_comments: i64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // TUI 占着整个终端，日志打出来会把界面写花 —— 那个模式下不装 subscriber
    if !matches!(cli.cmd, Cmd::Tui { .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| cli.log.clone().into()),
            )
            .with_target(false)
            .without_time()
            .init();
    }

    // schema 是纯本地操作，不需要配置和数据库
    if let Cmd::Schema = cli.cmd {
        println!("{}", phi_analyzer::schema::pretty());
        return Ok(());
    }

    let config = Config::load(cli.config.as_deref())?;

    // dbsync 要同时握着两个库，而且方向由参数决定、不看 [db] backend —— 所以它不走下面
    // 那条「按配置连一个库」的通用路径
    if let Cmd::Dbsync { reverse, dry_run } = cli.cmd {
        return dbsync(&config, reverse, dry_run).await;
    }

    let pool = db::connect(&config.db).await?;
    db::migrate(&pool).await?;
    let ctx = Ctx::new(config, pool);

    match cli.cmd {
        Cmd::Schema | Cmd::Dbsync { .. } => unreachable!(),

        Cmd::Tui { min_comments } => {
            // 缺 key 不影响浏览，只在按 a 分析时提示
            let backends = tui::Backends {
                source: build_source(&ctx)
                    .map(|s| Arc::new(s) as Arc<dyn phi_core::Source>)
                    .map_err(|e| format!("{e:#}")),
                analyzer: build_analyzer(&ctx, cli.model.clone())
                    .map(|a| Arc::new(a) as Arc<dyn phi_core::Analyzer>)
                    .map_err(|e| format!("{e:#}")),
            };
            tui::run(&ctx, min_comments, backends).await?
        }

        Cmd::Add { url } => {
            let source = build_source(&ctx)?;
            let analyzer = build_analyzer(&ctx, cli.model)?;
            let (item, analysis) = usecase::ingest_url(&ctx, &source, &analyzer, &url).await?;
            println!("\n{}", render::card_markdown(&item, &analysis));
        }

        Cmd::Sync {
            topic,
            since,
            until,
            max_pages,
            refresh,
        } => {
            let source = build_source(&ctx)?;
            let today = Utc::now().date_naive();
            let req = SyncRequest {
                topic,
                since: since
                    .as_deref()
                    .map(parse_day)
                    .transpose()?
                    .unwrap_or(today),
                until: until
                    .as_deref()
                    .map(parse_day)
                    .transpose()?
                    .unwrap_or(today),
                max_pages,
                refresh,
            };
            let r = usecase::sync(&ctx, &source, &req).await?;
            println!(
                "{} 天：新完成 {}，跳过（之前已拉完）{}，已拉但当天未结束 {}，未拉完 {}。",
                r.days,
                r.days_completed,
                r.days_skipped,
                r.days_open,
                r.days_pending()
            );
            println!(
                "请求 {} 次，处理 {} 条：新增 {}，更新 {}。",
                r.pages,
                r.inserted + r.updated,
                r.inserted,
                r.updated
            );
            if r.budget_exhausted {
                println!(
                    "已用完 --max-pages。已结束日期的进度已保存，重跑同一条命令会接着拉；\
                     还没结束的日期（今天）不存进度，下次从头拉。"
                );
            } else {
                println!("下一步：phi ls --min-comments 15 挑候选，再 phi hydrate 拉评论。");
            }
        }

        Cmd::Hydrate {
            min_comments,
            limit,
        } => {
            let source = build_source(&ctx)?;
            let n = usecase::hydrate(&ctx, &source, min_comments, limit).await?;
            println!("为 {n} 条 item 拉取了评论。");
        }

        Cmd::Analyze { item_id } => {
            let analyzer = build_analyzer(&ctx, cli.model)?;
            let analysis = usecase::analyze(&ctx, &analyzer, item_id).await?;
            let item = db::get_item(&ctx.db, item_id)
                .await?
                .ok_or_else(|| anyhow!("找不到 item {item_id}"))?;
            println!("\n{}", render::card_markdown(&item, &analysis));
        }

        Cmd::Reanalyze { analysis_id, diff } => {
            let analyzer = build_analyzer(&ctx, cli.model)?;
            let prev = db::get_analysis(&ctx.db, analysis_id)
                .await?
                .ok_or_else(|| anyhow!("找不到 analysis {analysis_id}"))?;
            let next = usecase::reanalyze(&ctx, &analyzer, analysis_id).await?;

            if diff {
                println!("{}", render::diff_cards(&prev, &next));
            } else {
                let item = db::get_item(&ctx.db, next.item_id)
                    .await?
                    .ok_or_else(|| anyhow!("找不到 item {}", next.item_id))?;
                println!("\n{}", render::card_markdown(&item, &next));
            }
        }

        Cmd::Diff { a, b } => {
            let ga = db::get_analysis(&ctx.db, a)
                .await?
                .ok_or_else(|| anyhow!("找不到 analysis {a}"))?;
            let gb = db::get_analysis(&ctx.db, b)
                .await?
                .ok_or_else(|| anyhow!("找不到 analysis {b}"))?;
            println!("{}", render::diff_cards(&ga, &gb));
        }

        Cmd::Ls {
            min_comments,
            limit,
            pending,
        } => {
            let analyzed = if pending { Some(false) } else { None };
            let items = db::list_items(&ctx.db, min_comments, limit, analyzed).await?;
            if items.is_empty() {
                println!("没有符合条件的条目。先跑 phi sync 或 phi add。");
            }
            for it in items {
                println!(
                    "{:>5}  {:>4}💬 {:>5}▲  {}",
                    it.id, it.signal_count, it.vote_count, it.name
                );
            }
        }

        Cmd::Show { item_id } => {
            let item = db::get_item(&ctx.db, item_id)
                .await?
                .ok_or_else(|| anyhow!("找不到 item {item_id}"))?;
            match db::latest_analysis(&ctx.db, item_id).await? {
                Some(a) => println!("{}", render::card_markdown(&item, &a)),
                None => println!("{} 还没有分析过。跑 phi analyze {item_id}", item.name),
            }
            let notes = db::load_notes(&ctx.db, item_id).await?;
            if !notes.is_empty() {
                println!("\n## 我的笔记\n");
                for (at, body) in notes {
                    println!("**{at}**\n\n{body}\n");
                }
            }
        }

        Cmd::Note { item_id, body } => {
            let body = match body {
                Some(b) => b,
                None => {
                    use std::io::Read;
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    s
                }
            };
            if body.trim().is_empty() {
                return Err(anyhow!("笔记内容是空的"));
            }
            let id = usecase::add_note(&ctx, item_id, body.trim()).await?;
            println!("笔记 #{id} 已记下。");
        }

        Cmd::Search { query, limit } => {
            let items = usecase::search(&ctx, &query, limit).await?;
            if items.is_empty() {
                println!("没有命中。");
            }
            for it in items {
                println!(
                    "{:>5}  {}  —  {}",
                    it.id,
                    it.name,
                    it.tagline.unwrap_or_default()
                );
            }
        }

        Cmd::Filtered { item_id } => {
            let comments = db::load_comments(&ctx.db, item_id, false).await?;
            print!("{}", render::filter_report(&comments));
            println!("\n--- 被滤掉的（抽查误杀）---\n");
            for c in comments.iter().filter(|c| !c.kept).take(25) {
                let preview: String = c.body.chars().take(90).collect();
                println!(
                    "[{}] {}",
                    c.filter_reason.as_deref().unwrap_or("?"),
                    preview.replace('\n', " ")
                );
            }
        }
    }

    Ok(())
}

/// `phi dbsync`。两端都跑一遍 migrations —— 目标库可能还是空的。
async fn dbsync(config: &phi_core::Config, reverse: bool, dry_run: bool) -> Result<()> {
    let sqlite = db::connect_sqlite(&config.db.path, config.db.max_connections)
        .await
        .with_context(|| format!("打开 SQLite 失败: {}", config.db.path.display()))?;
    db::migrate(&sqlite).await?;
    let mysql =
        db::connect_mysql(&config.db.resolve_mysql_url()?, config.db.max_connections).await?;
    db::migrate(&mysql).await?;

    let (from, to, arrow) = if reverse {
        (&mysql, &sqlite, "MySQL → SQLite")
    } else {
        (&sqlite, &mysql, "SQLite → MySQL")
    };
    db::transfer::reject_same_endpoint(from, to)?;

    println!(
        "{arrow}{}\n只插入目标库缺少的行，已有的行不改、多出来的行不删。\n",
        if dry_run {
            "（dry-run，不写任何数据）"
        } else {
            ""
        }
    );

    let rep = db::transfer::run(from, to, dry_run).await?;
    for line in rep.lines() {
        println!("{line}");
    }
    let n = rep.inserted_total();
    println!();
    if dry_run {
        println!("dry-run：会插入 {n} 行。去掉 --dry-run 真正执行。");
    } else if n == 0 {
        println!("目标库已经是最新的，没有需要插入的行。");
    } else {
        println!("插入 {n} 行。再跑一次这条命令会是 0 —— 它是幂等的。");
    }
    if rep.orphans > 0 {
        println!(
            "有 {} 行的外键指向不存在的父行，已跳过（源库自身的数据问题）。",
            rep.orphans
        );
    }
    Ok(())
}

fn build_source(ctx: &Ctx) -> Result<ProductHunt> {
    let token = Config::secret(&ctx.config.producthunt.token_env).context(
        "ProductHunt token 没配。到 https://www.producthunt.com/v2/oauth/applications \
         生成一个 Developer Token（不过期），然后 export PH_TOKEN=...",
    )?;
    ProductHunt::new(&ctx.config.producthunt, token)
}

fn build_analyzer(ctx: &Ctx, model_override: Option<String>) -> Result<OpenRouterAnalyzer> {
    let key = Config::secret(&ctx.config.analyzer.api_key_env)
        .context("OpenRouter API key 没配。export OPENROUTER_API_KEY=...")?;
    OpenRouterAnalyzer::new(&ctx.config.analyzer, key, model_override)
}

fn parse_day(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("日期格式应为 YYYY-MM-DD，收到: {s}"))
}
