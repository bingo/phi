//! 只管画。三栏：左列表（可筛可排）、中卡片、右笔记。
//!
//! 笔记独立一栏而不是卡片里的一个字段 —— 「分析和笔记两条线分离」在 UI 上的体现。

use chrono::{DateTime, Local};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use phi_core::model::{AnalysisState, SortKey, Tri};
use phi_core::render::verdict_label;

use phi_core::usecase::AnalyzeStage;
use std::time::Instant;

use super::app::{App, Focus, Job, Mode};
use super::card::{self, tri_color, verdict_color, verdict_short};

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [left, mid, right] = Layout::horizontal([
        Constraint::Percentage(30),
        Constraint::Percentage(44),
        Constraint::Percentage(26),
    ])
    .areas(body);

    draw_header(f, app, header);
    draw_list(f, app, left);
    draw_card(f, app, mid);
    draw_notes(f, app, right);
    draw_footer(f, app, footer);

    if app.mode == Mode::Help {
        draw_help(f);
    }
}

fn pane(title: String, focused: bool) -> Block<'static> {
    let color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    Block::bordered()
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(Style::new().fg(color))
        .title(Span::styled(
            title,
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ))
}

// ---------------------------------------------------------------- 顶栏

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let q = &app.query;
    let chip = |s: String| {
        Span::styled(
            format!(" {s} "),
            Style::new().fg(Color::Black).bg(Color::Cyan),
        )
    };
    let mut spans = vec![
        Span::styled(
            " phi ",
            Style::new().fg(Color::Black).bg(Color::White).bold(),
        ),
        Span::raw(" "),
        Span::raw(format!("评论≥{}", q.min_signal)),
        Span::raw("  "),
        Span::raw(format!(
            "排序:{}",
            match q.sort {
                SortKey::Comments => "评论数",
                SortKey::Votes => "票数",
                SortKey::Newest => "最新",
                SortKey::Name => "名称",
            }
        )),
        Span::raw("  "),
    ];
    match q.state {
        AnalysisState::All => {}
        AnalysisState::Analyzed => spans.extend([chip("已分析".into()), Span::raw(" ")]),
        AnalysisState::Pending => spans.extend([chip("未分析".into()), Span::raw(" ")]),
    }
    if let Some(v) = q.verdict {
        spans.extend([chip(format!("结论:{}", verdict_label(v))), Span::raw(" ")]);
    }
    for (label, t) in [
        ("能做", q.buildable),
        ("值得", q.worth_it),
        ("触达", q.reachable),
    ] {
        if let Some(t) = t {
            spans.extend([chip(format!("{label}:{}", tri_word(t))), Span::raw(" ")]);
        }
    }
    if app.mode == Mode::Search {
        spans.push(Span::styled(
            format!("/{}▏", app.search_input),
            Style::new().fg(Color::Yellow).bold(),
        ));
    } else if let Some(t) = &q.text {
        spans.push(chip(format!("搜索:{t}")));
    }
    if !app.jobs.is_empty() {
        let jobs: Vec<String> = app.jobs.iter().map(job_label).collect();
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(
                "{} 分析中 {}：{}",
                spinner(app.jobs[0].started),
                app.jobs.len(),
                jobs.join(" · ")
            ),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Line::from(spans), area);
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner(started: Instant) -> &'static str {
    SPINNER[(started.elapsed().as_millis() / 100) as usize % SPINNER.len()]
}

fn job_label(j: &Job) -> String {
    let stage = match j.stage {
        None => "准备",
        Some(AnalyzeStage::FetchingComments) => "抓评论",
        Some(AnalyzeStage::CallingModel) => "调用模型",
    };
    format!("{} {stage} {}s", j.name, j.started.elapsed().as_secs())
}

fn tri_word(t: Tri) -> &'static str {
    match t {
        Tri::Yes => "是",
        Tri::Unsure => "存疑",
        Tri::No => "否",
    }
}

// ---------------------------------------------------------------- 左：列表

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let block = pane(
        format!(" 列表 {}/{} ", app.rows.len(), app.total),
        app.focus == Focus::List,
    );

    if app.rows.is_empty() {
        let msg = if app.total == 0 {
            "库是空的。\n\n先在终端跑\nphi add <url>\n或 phi sync"
        } else {
            "没有符合条件的条目。\n\n按 x 清空筛选，\n或 - 降低评论阈值。"
        };
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::new().fg(Color::DarkGray))
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }

    let axis = |t: Option<Tri>| match t {
        Some(t) => Span::styled(
            match t {
                Tri::Yes => "✓",
                Tri::Unsure => "?",
                Tri::No => "✗",
            },
            Style::new().fg(tri_color(t)),
        ),
        None => Span::styled("·", Style::new().fg(Color::DarkGray)),
    };

    let rows = app.rows.iter().map(|r| {
        let verdict = match r.verdict {
            Some(v) => Span::styled(verdict_short(v), Style::new().fg(verdict_color(v)).bold()),
            None => Span::styled("—", Style::new().fg(Color::DarkGray)),
        };
        let mut name = vec![Span::raw(r.item.name.clone())];
        if let Some(j) = app.job_for(r.item.id) {
            name.push(Span::styled(
                format!(" {}", spinner(j.started)),
                Style::new().fg(Color::Yellow),
            ));
        }
        if r.note_count > 0 {
            name.push(Span::styled(" ✎", Style::new().fg(Color::Magenta)));
        }
        if r.analysis_count > 1 {
            name.push(Span::styled(
                format!(" ×{}", r.analysis_count),
                Style::new().fg(Color::DarkGray),
            ));
        }
        Row::new(vec![
            Cell::from(Line::from(verdict)),
            Cell::from(Line::from(vec![
                axis(r.buildable),
                axis(r.worth_it),
                axis(r.reachable),
            ])),
            Cell::from(Line::from(format!("{}", r.item.signal_count)).alignment(Alignment::Right)),
            Cell::from(Line::from(name)),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Fill(1),
        ],
    )
    .header(Row::new(["结", "轴", "评论", "名称"]).style(Style::new().fg(Color::DarkGray)))
    .row_highlight_style(
        Style::new()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .block(block);

    f.render_stateful_widget(table, area, &mut app.table);
}

// ---------------------------------------------------------------- 中：卡片

fn draw_card(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Card;
    let Some(detail) = app.detail.as_ref() else {
        f.render_widget(
            Paragraph::new("").block(pane(" 卡片 ".into(), focused)),
            area,
        );
        return;
    };

    let n = detail.analyses.len();
    let (mut title, mut lines) = match detail.analyses.get(app.version) {
        Some(a) => {
            let verdict = a.card.verdict;
            let version = if n > 1 {
                format!(" · 版本 {}/{n}  [ ] 切换", n - app.version)
            } else {
                String::new()
            };
            (
                format!(" 卡片 · {}{version} ", verdict_label(verdict)),
                card::card_lines(&detail.item, a),
            )
        }
        None => {
            let mut lines = card::item_header(&detail.item);
            lines.push(Line::default());
            let hint = if detail.item.comments_fetched_at.is_some() {
                "还没有分析过。按 a 分析"
            } else {
                "还没有分析过，评论也还没抓。按 a 先抓评论再分析"
            };
            lines.push(Line::from(Span::styled(
                hint,
                Style::new().fg(Color::Yellow),
            )));
            if let Some(d) = detail.item.description.as_deref() {
                lines.push(Line::default());
                lines.extend(d.lines().map(|l| Line::from(l.to_string())));
            }
            (" 卡片 · 未分析 ".to_string(), lines)
        }
    };

    if let Some(j) = app.job_for(detail.item.id) {
        title = format!(" 卡片 · {} 分析中 ", spinner(j.started));
        lines.splice(
            0..0,
            [
                Line::from(Span::styled(
                    format!("{} 正在后台分析：{}", spinner(j.started), job_label(j)),
                    Style::new().fg(Color::Yellow).bold(),
                )),
                Line::default(),
            ],
        );
    }

    let block = pane(title, focused);
    let inner = block.inner(area);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let max = (para.line_count(inner.width) as u16).saturating_sub(inner.height);
    app.card_scroll = app.card_scroll.min(max);
    app.card_page = inner.height;
    f.render_widget(para.scroll((app.card_scroll, 0)).block(block), area);
}

// ---------------------------------------------------------------- 右：笔记

fn draw_notes(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Notes;
    let count = app.detail.as_ref().map_or(0, |d| d.notes.len());

    let list_area = if app.mode == Mode::NoteEdit {
        let editor_h = (area.height * 2 / 5).max(8).min(area.height);
        let [editor_area, rest] =
            Layout::vertical([Constraint::Length(editor_h), Constraint::Fill(1)]).areas(area);
        app.editor.set_block(
            Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(Color::Magenta))
                .title(" 新笔记 · Ctrl+S 保存 · Esc 放弃 ".magenta().bold()),
        );
        f.render_widget(&app.editor, editor_area);
        rest
    } else {
        area
    };

    let block = pane(
        format!(" 我的笔记 ({count}) "),
        focused && app.mode != Mode::NoteEdit,
    );
    let lines: Vec<Line> = match app.detail.as_ref() {
        None => vec![],
        Some(d) if d.notes.is_empty() => ["还没有笔记。", "按 n 写一条，E 用外部编辑器。"]
            .into_iter()
            .map(|s| Line::from(Span::styled(s, Style::new().fg(Color::DarkGray))))
            .collect(),
        Some(d) => d
            .notes
            .iter()
            .flat_map(|(at, body)| {
                let mut v = vec![Line::from(Span::styled(
                    local_time(at),
                    Style::new().fg(Color::Magenta).bold(),
                ))];
                v.extend(body.lines().map(|l| Line::from(l.to_string())));
                v.push(Line::default());
                v
            })
            .collect(),
    };

    let inner = block.inner(list_area);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let max = (para.line_count(inner.width) as u16).saturating_sub(inner.height);
    app.notes_scroll = app.notes_scroll.min(max);
    app.notes_page = inner.height;
    f.render_widget(para.scroll((app.notes_scroll, 0)).block(block), list_area);
}

fn local_time(rfc3339: &str) -> String {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}

// ---------------------------------------------------------------- 底栏 / 帮助

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = match (&app.status, app.mode) {
        (Some(s), _) => Line::from(Span::styled(
            format!(" {}", s.text),
            Style::new().fg(if s.is_error { Color::Red } else { Color::Green }),
        )),
        (None, Mode::Search) => hints(&[("Enter", "确定"), ("Esc", "取消"), ("Ctrl+U", "清空")]),
        (None, Mode::NoteEdit) => hints(&[("Ctrl+S / Alt+Enter", "保存"), ("Esc", "放弃")]),
        (None, _) => hints(&[
            ("q", "退出"),
            ("?", "帮助"),
            ("Tab", "切栏"),
            ("j/k", "移动"),
            ("/", "搜索"),
            ("s", "排序"),
            ("f v b w r", "筛选"),
            ("+/-", "阈值"),
            ("n", "笔记"),
            ("a", "分析"),
            ("[ ]", "版本"),
        ]),
    };
    f.render_widget(line, area);
}

fn hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (k, v) in pairs {
        spans.push(Span::styled(
            k.to_string(),
            Style::new().fg(Color::Cyan).bold(),
        ));
        spans.push(Span::styled(
            format!(" {v}  "),
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

const HELP: &[(&str, &str)] = &[
    ("", "── 移动"),
    ("Tab / h l", "切换栏：列表 → 卡片 → 笔记"),
    ("j k ↑ ↓", "列表里移动选中；卡片 / 笔记栏里滚动"),
    ("Space PgDn / PgUp", "翻页（Ctrl+D / Ctrl+U 同）"),
    ("g / G", "到顶 / 到底"),
    ("", "── 筛选（全是视图层，不重拉数据）"),
    ("/", "搜索：名称、描述、卡片正文、笔记（实时，支持中文）"),
    ("Esc", "清除搜索"),
    ("+ / -", "评论数阈值 ±5"),
    ("f", "全部 → 已分析 → 未分析"),
    ("v", "结论：跟进 → 观望 → 放弃 → 不限"),
    (
        "b / w / r",
        "做得出来 / 值得做 / 卖得出去：是 → 存疑 → 否 → 不限",
    ),
    ("s", "排序：评论数 → 票数 → 最新 → 名称"),
    ("x", "清空全部筛选"),
    ("", "── 卡片与笔记"),
    (
        "[ / ]",
        "切到更旧 / 更新的分析版本（analysis 只追加，旧版本都在）",
    ),
    ("n", "在笔记栏里写一条新笔记（Ctrl+S 保存）"),
    ("E", "用 $VISUAL / $EDITOR 写笔记"),
    ("a", "分析选中的条目（后台跑，没抓评论的先抓；需要确认）"),
    ("o", "在浏览器里打开产品页"),
    ("R", "从数据库重新加载（别的终端跑完 phi add 之后）"),
    ("q", "退出"),
];

fn draw_help(f: &mut Frame) {
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, v)| {
            if k.is_empty() {
                Line::from(Span::styled(
                    v.to_string(),
                    Style::new().fg(Color::Cyan).bold(),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("{k:>18}  "), Style::new().fg(Color::Yellow)),
                    Span::raw(v.to_string()),
                ])
            }
        })
        .collect();

    let h = (lines.len() as u16 + 2).min(f.area().height);
    let [area] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::horizontal([Constraint::Max(84)])
        .flex(Flex::Center)
        .areas(area);

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(Color::Cyan))
                .title(" 按键 · 任意键关闭 ".cyan().bold()),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use phi_core::model::{ItemDetail, Overview, Verdict};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;

    /// 把缓冲区拍成字符串。去掉所有空格再比较，避开宽字符占两格带来的空格问题
    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn has(screen: &str, needle: &str) -> bool {
        let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        squash(screen).contains(&squash(needle))
    }

    fn loaded_app() -> App {
        let mut app = App::new(0);
        let mut noted = summary(1, "Caddi", 41, Some(Verdict::Watch));
        noted.note_count = 1;
        app.set_rows(Overview {
            rows: vec![noted, summary(2, "Zeta", 20, None)],
            total: 3,
        });
        app.set_detail(ItemDetail {
            item: item(1, "Caddi", 41),
            analyses: vec![
                analysis(12, 1, Verdict::Watch),
                analysis(11, 1, Verdict::Drop),
            ],
            notes: vec![("2026-09-15T10:00:00+00:00".into(), "获客才是真问题".into())],
        });
        app
    }

    #[test]
    fn renders_three_panes_with_card_and_notes() {
        let mut app = loaded_app();
        let s = screen(&mut app, 160, 60);

        assert!(has(&s, "列表 2/3"), "{s}");
        assert!(has(&s, "Caddi ✎"), "有笔记的条目要有标记\n{s}");
        assert!(has(&s, "卡片 · 观望 · 版本 2/2"), "{s}");
        assert!(has(&s, "做得出来吗 [存疑]"), "{s}");
        assert!(has(&s, "结论：观望"), "{s}");
        assert!(
            has(&s, "│ That's usually where automations break for me."),
            "证据引文要原样出现\n{s}"
        );
        assert!(has(&s, "这是个坑"), "{s}");
        assert!(has(&s, "via Parasail"), "provider 要可见\n{s}");
        assert!(has(&s, "我的笔记 (1)"), "{s}");
        assert!(has(&s, "获客才是真问题"), "{s}");

        // 切到旧版本
        app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        let s = screen(&mut app, 160, 60);
        assert!(has(&s, "卡片 · 放弃 · 版本 1/2"), "{s}");
    }

    #[test]
    fn renders_filters_search_editor_and_help() {
        let mut app = loaded_app();
        app.query.verdict = Some(Verdict::Watch);
        app.query.text = Some("律所".into());
        let s = screen(&mut app, 160, 40);
        assert!(has(&s, "结论:观望"), "{s}");
        assert!(has(&s, "搜索:律所"), "{s}");

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        let s = screen(&mut app, 160, 40);
        assert!(has(&s, "新笔记 · Ctrl+S 保存"), "{s}");

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        let s = screen(&mut app, 160, 40);
        assert!(has(&s, "按键 · 任意键关闭"), "{s}");
    }

    #[test]
    fn renders_running_analysis_everywhere() {
        let mut app = loaded_app();
        app.job_started(1, "Caddi".into());
        app.job_stage(1, phi_core::usecase::AnalyzeStage::CallingModel);
        app.status = None;
        let s = screen(&mut app, 160, 40);
        assert!(has(&s, "分析中 1：Caddi 调用模型"), "顶栏要显示进度\n{s}");
        assert!(has(&s, "卡片 ·"), "{s}");
        assert!(
            has(&s, "正在后台分析：Caddi 调用模型"),
            "卡片栏要显示进度\n{s}"
        );
        assert!(
            SPINNER.iter().any(|sp| has(&s, &format!("Caddi {sp}"))),
            "列表行要有转圈标记\n{s}"
        );

        // 未分析的条目提示按 a
        app.jobs.clear();
        app.set_detail(ItemDetail {
            item: item(2, "Zeta", 20),
            analyses: vec![],
            notes: vec![],
        });
        let s = screen(&mut app, 160, 40);
        assert!(has(&s, "按 a 先抓评论再分析"), "{s}");
    }

    #[test]
    fn empty_and_narrow_terminals_do_not_panic() {
        let mut app = App::new(0);
        let s = screen(&mut app, 100, 30);
        assert!(has(&s, "库是空的"), "{s}");

        let mut app = loaded_app();
        for (w, h) in [(40, 10), (20, 5), (1, 1)] {
            screen(&mut app, w, h);
        }
        // 卡片滚到底之后不能滚出界
        app.card_scroll = u16::MAX;
        screen(&mut app, 160, 40);
        assert!(app.card_scroll < 200, "滚动没被夹住: {}", app.card_scroll);
    }
}
