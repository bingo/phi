//! `phi` —— ProductHunt inspiration tool.
//!
//! 所有子命令都是 `phi_core::usecase` 里函数的薄包装。业务流程不写在这里。

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use phi_analyzer::OpenRouterAnalyzer;
use phi_core::model::ListQuery;
use phi_core::{config::Config, db, render, usecase, Ctx};
use phi_sources::ProductHunt;

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

    /// 阶段一：按分类/日期批量拉轻量元数据入库（不碰评论、不做分析）
    Sync {
        #[arg(long)]
        topic: Option<String>,
        /// 起始日期，形如 2026-09-01
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: usize,
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

    /// 打印发给模型的 JSON Schema（调试用）
    Schema,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log.clone().into()),
        )
        .with_target(false)
        .without_time()
        .init();

    // schema 是纯本地操作，不需要配置和数据库
    if let Cmd::Schema = cli.cmd {
        println!("{}", phi_analyzer::schema::pretty());
        return Ok(());
    }

    let config = Config::load(cli.config.as_deref())?;
    let pool = db::connect(&config.db.path).await?;
    db::migrate(&pool).await?;
    let ctx = Ctx::new(config, pool);

    match cli.cmd {
        Cmd::Schema => unreachable!(),

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
            limit,
        } => {
            let source = build_source(&ctx)?;
            let q = ListQuery {
                topic,
                posted_after: since.as_deref().map(parse_date).transpose()?,
                posted_before: until.as_deref().map(parse_date).transpose()?,
                limit,
            };
            let n = usecase::sync(&ctx, &source, &q).await?;
            println!("入库 {n} 条。");
            println!("下一步：phi ls --min-comments 15 挑候选，再 phi hydrate 拉评论。");
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
                println!("{:>5}  {}  —  {}", it.id, it.name, it.tagline.unwrap_or_default());
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

fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("日期格式应为 YYYY-MM-DD，收到: {s}"))?;
    Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
}
