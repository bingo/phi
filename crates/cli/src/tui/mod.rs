//! `phi tui` —— 三栏浏览：左列表（可筛可排）、中卡片、右笔记。
//!
//! 和其它子命令一样只是 `phi_core::usecase` 的薄包装，不含业务流程。
//!
//! - `app`：纯状态机，按键 → 状态变化 + [`app::Effect`]。不碰终端和数据库，可以直接测
//! - `ui` / `card`：只管画
//! - 本文件：终端生命周期，执行 `Effect` 里的 IO，以及后台分析任务

mod app;
mod card;
mod ui;

use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::DefaultTerminal;
use std::collections::VecDeque;
use std::io::stdout;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use phi_core::usecase::AnalyzeStage;
use phi_core::{usecase, Analyzer, Ctx, Source};

use app::{App, Effect};

/// 分析要用到的外部依赖。缺 key 时 TUI 照样能浏览，只是按 `a` 会提示原因。
pub struct Backends {
    pub source: Result<Arc<dyn Source>, String>,
    pub analyzer: Result<Arc<dyn Analyzer>, String>,
}

/// 后台分析任务发回事件循环的消息
enum JobEvent {
    Stage(i64, AnalyzeStage),
    /// 成功是新 analysis 的 id
    Done(i64, Result<i64, String>),
}

/// 有后台任务时要定时重绘（转圈和计时），所以读事件不能无限阻塞
const TICK: Duration = Duration::from_millis(150);

pub async fn run(ctx: &Ctx, min_comments: i64, backends: Backends) -> Result<()> {
    let mut app = App::new(min_comments);
    // 第一次加载放在进终端之前：库打不开之类的错误能正常打印到 shell
    reload(ctx, &mut app).await?;

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableBracketedPaste).ok();
    let result = event_loop(&mut terminal, ctx, &backends, &mut app).await;
    execute!(stdout(), DisableBracketedPaste).ok();
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    ctx: &Ctx,
    backends: &Backends,
    app: &mut App,
) -> Result<()> {
    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel::<JobEvent>();
    let mut queue: VecDeque<Effect> = VecDeque::new();

    loop {
        // ---- 后台任务的进度
        while let Ok(ev) = jobs_rx.try_recv() {
            match ev {
                JobEvent::Stage(id, stage) => app.job_stage(id, stage),
                JobEvent::Done(id, result) => {
                    app.job_finished(id, result);
                    // 列表里的结论 / 三轴、卡片栏的新版本都要刷新
                    queue.push_back(Effect::Reload);
                }
            }
        }

        // ---- 执行 effect
        while let Some(effect) = queue.pop_front() {
            match effect {
                Effect::None => {}
                Effect::Quit => return Ok(()),
                Effect::Batch(effects) => {
                    for e in effects.into_iter().rev() {
                        queue.push_front(e);
                    }
                }
                Effect::Reload => {
                    if let Err(e) = reload(ctx, app).await {
                        app.error(format!("加载失败：{e:#}"));
                    }
                }
                Effect::LoadDetail(id) => {
                    if let Err(e) = load_detail(ctx, app, id).await {
                        app.error(format!("加载详情失败：{e:#}"));
                    }
                }
                Effect::SaveNote { item_id, body } => {
                    // 失败时编辑器内容原样保留，不会丢
                    match save_note(ctx, app, item_id, &body).await {
                        Ok(()) => app.note_saved(),
                        Err(e) => app.error(format!("笔记保存失败（内容还在编辑器里）：{e:#}")),
                    }
                }
                Effect::ExternalEditor { item_id } => {
                    suspend()?;
                    let edited = edit_externally();
                    resume(terminal)?;
                    match edited {
                        Ok(Some(body)) => {
                            if let Err(e) = save_note(ctx, app, item_id, &body).await {
                                app.error(format!("笔记保存失败：{e:#}"));
                            }
                        }
                        Ok(None) => app.info("笔记是空的，没有保存"),
                        Err(e) => app.error(format!("{e:#}")),
                    }
                }
                Effect::OpenUrl(url) => {
                    let opener = if cfg!(target_os = "macos") {
                        "open"
                    } else {
                        "xdg-open"
                    };
                    match Command::new(opener).arg(&url).spawn() {
                        Ok(_) => app.info(format!("已在浏览器打开 {url}")),
                        Err(e) => app.error(format!("打不开浏览器（{opener}）：{e}")),
                    }
                }
                Effect::StartAnalyze { item_id } => {
                    start_analyze(ctx, backends, app, item_id, jobs_tx.clone());
                }
            }
        }

        terminal.draw(|f| ui::draw(f, app))?;

        // block_in_place 避免占住 tokio 的 worker；后台分析任务在别的 worker 上跑
        if tokio::task::block_in_place(|| event::poll(TICK))? {
            let ev = event::read()?;
            queue.push_back(app.handle_event(ev));
        }
    }
}

/// 把分析丢到后台任务里跑，进度和结果通过 channel 发回事件循环。
fn start_analyze(
    ctx: &Ctx,
    backends: &Backends,
    app: &mut App,
    item_id: i64,
    tx: mpsc::UnboundedSender<JobEvent>,
) {
    let analyzer = match &backends.analyzer {
        Ok(a) => a.clone(),
        Err(e) => {
            app.error(format!("没法分析：{e}"));
            return;
        }
    };
    // 信息源只在需要先抓评论时才用到，缺了不一定影响
    let source = backends.source.as_ref().ok().cloned();
    let name = app
        .rows
        .iter()
        .find(|r| r.item.id == item_id)
        .map(|r| r.item.name.clone())
        .unwrap_or_else(|| format!("#{item_id}"));
    app.job_started(item_id, name);

    let ctx = ctx.clone();
    tokio::spawn(async move {
        let stage_tx = tx.clone();
        let on_stage = move |stage| {
            let _ = stage_tx.send(JobEvent::Stage(item_id, stage));
        };
        let result = usecase::analyze_item(
            &ctx,
            source.as_deref(),
            analyzer.as_ref(),
            item_id,
            &on_stage,
        )
        .await
        .map(|a| a.id)
        .map_err(|e| format!("{e:#}"));
        // 事件循环已经退出时发送会失败，忽略即可
        let _ = tx.send(JobEvent::Done(item_id, result));
    });
}

async fn reload(ctx: &Ctx, app: &mut App) -> Result<()> {
    let overview = usecase::overview(ctx, &app.query).await?;
    app.set_rows(overview);
    // 选中的没变也刷新详情：可能是按了 R，别的终端刚跑完一次 reanalyze。本地 SQLite，很便宜
    match app.selected_id() {
        Some(id) => load_detail(ctx, app, id).await,
        None => Ok(()),
    }
}

async fn load_detail(ctx: &Ctx, app: &mut App, item_id: i64) -> Result<()> {
    let detail = usecase::item_detail(ctx, item_id).await?;
    app.set_detail(detail);
    Ok(())
}

async fn save_note(ctx: &Ctx, app: &mut App, item_id: i64, body: &str) -> Result<()> {
    let id = usecase::add_note(ctx, item_id, body).await?;
    // 列表里的 ✎ 标记和笔记栏都要更新
    let overview = usecase::overview(ctx, &app.query).await?;
    app.set_rows(overview);
    load_detail(ctx, app, item_id).await?;
    app.info(format!("笔记 #{id} 已保存"));
    Ok(())
}

fn suspend() -> Result<()> {
    execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

fn resume(terminal: &mut DefaultTerminal) -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    terminal.clear()?;
    Ok(())
}

/// 用 `$VISUAL` / `$EDITOR` 写一条笔记。返回 `None` 表示内容为空。
fn edit_externally() -> Result<Option<String>> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let path = std::env::temp_dir().join(format!("phi-note-{}.md", std::process::id()));
    std::fs::write(&path, "")?;

    // 编辑器可能带参数，比如 `code --wait`
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let status = match Command::new(program).args(parts).arg(&path).status() {
        Ok(s) => s,
        Err(e) => bail!("启动编辑器 `{editor}` 失败：{e}"),
    };
    let body = std::fs::read_to_string(&path)?;
    if !status.success() {
        // 保留临时文件，写了一半的内容还能找回来
        bail!(
            "编辑器 `{editor}` 异常退出（{status}），笔记未保存。草稿在 {}",
            path.display()
        );
    }
    let _ = std::fs::remove_file(&path);
    let body = body.trim();
    Ok((!body.is_empty()).then(|| body.to_string()))
}

#[cfg(test)]
mod fixtures {
    use phi_core::model::*;

    pub fn item(id: i64, name: &str, comments: i64) -> Item {
        Item {
            id,
            source: "producthunt".into(),
            source_id: format!("p{id}"),
            slug: Some(name.to_lowercase()),
            url: format!("https://www.producthunt.com/posts/{}", name.to_lowercase()),
            name: name.into(),
            tagline: Some(format!("{name} tagline")),
            description: Some("desc".into()),
            website: None,
            posted_at: Some("2026-09-01T00:00:00Z".into()),
            signal_count: comments,
            vote_count: 100,
            topics: vec!["Legal".into(), "Artificial Intelligence".into()],
            fetched_at: "2026-09-15T00:00:00Z".into(),
            comments_fetched_at: None,
        }
    }

    pub fn summary(id: i64, name: &str, comments: i64, verdict: Option<Verdict>) -> ItemSummary {
        ItemSummary {
            item: item(id, name, comments),
            latest_analysis_id: verdict.map(|_| id * 10),
            verdict,
            buildable: verdict.map(|_| Tri::Unsure),
            worth_it: verdict.map(|_| Tri::Unsure),
            reachable: verdict.map(|_| Tri::No),
            analysis_count: verdict.map_or(0, |_| 1),
            note_count: 0,
        }
    }

    pub fn overview(rows: Vec<ItemSummary>) -> Overview {
        let total = rows.len();
        Overview { rows, total }
    }

    pub fn analysis(id: i64, item_id: i64, verdict: Verdict) -> Analysis {
        let axis = |value, reason: &str| Axis {
            value,
            reason: reason.into(),
        };
        Analysis {
            id,
            item_id,
            created_at: "2026-09-15T10:35:50+00:00".into(),
            prompt_version: "v1".into(),
            model: "deepseek/deepseek-v4.1-flash".into(),
            provider: Some("Parasail".into()),
            card: OpportunityCard {
                one_liner: "把录屏变成 back-office agent".into(),
                buildable: axis(Tri::Unsure, "demo 容易，生产级难"),
                worth_it: axis(Tri::Unsure, "付费证据为零"),
                reachable: axis(Tri::No, "买方不在 ProductHunt 上"),
                pain: Some("重复性的文档归档".into()),
                pain_evidence: vec![Evidence {
                    quote: "That's usually where automations break for me.".into(),
                    source_ref: "评论 votes=2".into(),
                }],
                who_pays: None,
                who_pays_evidence: vec![],
                build_cost: "3-6 人月".into(),
                moat: "无".into(),
                distribution: "BD".into(),
                business_model: "按席位".into(),
                gap: None,
                gap_evidence: vec![],
                competitors: vec![Competitor {
                    name: "UiPath".into(),
                    difference: "通用 RPA".into(),
                    how_i_know: "厂商描述点名".into(),
                }],
                competitors_note: String::new(),
                trap: "信任不是 3 个月能写出来的代码".into(),
                verdict,
                verdict_reason: "两轴存疑一轴否".into(),
                insufficient_evidence: vec!["who_pays".into()],
            },
            tokens_in: Some(2359),
            tokens_out: Some(4712),
            cost_usd: Some(0.0062),
        }
    }
}
