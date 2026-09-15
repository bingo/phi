//! 卡片 → 带样式的行。结构和 `phi_core::render::card_markdown` 保持一致，标签也复用那边的。
//!
//! 和 markdown 版一样，刻意把证据紧贴在结论下面：没有证据的判断是负资产，排版要让这件事一眼可见。

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use phi_core::model::{Analysis, Evidence, Item, Tri, Verdict};
use phi_core::render::{tri_mark, verdict_label};

pub fn tri_color(t: Tri) -> Color {
    match t {
        Tri::Yes => Color::Green,
        Tri::Unsure => Color::Yellow,
        Tri::No => Color::Red,
    }
}

pub fn verdict_color(v: Verdict) -> Color {
    match v {
        Verdict::Follow => Color::Green,
        Verdict::Watch => Color::Yellow,
        Verdict::Drop => Color::Red,
    }
}

/// 列表和标题里用的一字简称
pub fn verdict_short(v: Verdict) -> &'static str {
    match v {
        Verdict::Follow => "跟",
        Verdict::Watch => "观",
        Verdict::Drop => "弃",
    }
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("── {text} "),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))
}

fn text_lines(text: &str) -> impl Iterator<Item = Line<'static>> + '_ {
    text.lines().map(|l| Line::from(l.to_string()))
}

pub fn item_header(item: &Item) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(item.name.clone().bold())];
    if let Some(t) = item.tagline.as_deref().filter(|t| !t.is_empty()) {
        out.push(Line::from(Span::styled(t.to_string(), dim())));
    }
    out.push(Line::from(Span::styled(
        format!(
            "#{} · {} 票 · {} 条评论 · {}",
            item.id,
            item.vote_count,
            item.signal_count,
            item.posted_at
                .as_deref()
                .map(|p| p.get(..10).unwrap_or(p))
                .unwrap_or("发布时间未知")
        ),
        dim(),
    )));
    if !item.topics.is_empty() {
        out.push(Line::from(Span::styled(item.topics.join(" · "), dim())));
    }
    out
}

pub fn card_lines(item: &Item, analysis: &Analysis) -> Vec<Line<'static>> {
    let card = &analysis.card;
    let mut out = item_header(item);
    out.push(Line::default());
    out.push(Line::from(Span::styled(
        card.one_liner.clone(),
        Style::new().add_modifier(Modifier::ITALIC),
    )));
    out.push(Line::default());

    // ---- 三轴
    out.push(heading("判断"));
    for (label, axis) in [
        ("做得出来吗", &card.buildable),
        ("值得做吗　", &card.worth_it),
        ("卖得出去吗", &card.reachable),
    ] {
        out.push(Line::from(vec![
            Span::raw(format!("{label} ")),
            Span::styled(
                tri_mark(axis.value),
                Style::new()
                    .fg(tri_color(axis.value))
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        out.extend(axis.reason.lines().map(|l| Line::from(format!("  {l}"))));
    }
    out.push(Line::default());
    out.push(Line::from(vec![
        Span::styled(
            format!("结论：{}", verdict_label(card.verdict)),
            Style::new()
                .fg(verdict_color(card.verdict))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" — {}", card.verdict_reason)),
    ]));
    out.push(Line::default());

    // ---- 带证据的三栏
    out.push(heading("需求"));
    for (title, body, evidence) in [
        ("痛点", &card.pain, &card.pain_evidence),
        ("谁付钱", &card.who_pays, &card.who_pays_evidence),
        ("空位", &card.gap, &card.gap_evidence),
    ] {
        out.push(Line::from(title.bold()));
        match body.as_deref().filter(|b| !b.trim().is_empty()) {
            Some(b) => out.extend(text_lines(b)),
            None => out.push(Line::from(Span::styled(
                "证据不足，留空",
                dim().add_modifier(Modifier::ITALIC),
            ))),
        }
        push_evidence(&mut out, evidence);
        out.push(Line::default());
    }

    // ---- 可行性
    out.push(heading("可行性"));
    for (label, text) in [
        ("复刻门槛", &card.build_cost),
        ("护城河", &card.moat),
        ("获客渠道", &card.distribution),
        ("商业模式", &card.business_model),
    ] {
        out.push(Line::from(vec![
            Span::styled(
                format!("{label}　"),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw(text.clone()),
        ]));
    }
    out.push(Line::default());

    // ---- 竞品
    out.push(heading("竞品"));
    if card.competitors.is_empty() {
        out.push(Line::from(Span::styled(
            "未列出",
            dim().add_modifier(Modifier::ITALIC),
        )));
    }
    for c in &card.competitors {
        out.push(Line::from(vec![
            Span::styled(
                format!("• {}", c.name),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" — {}", c.difference)),
        ]));
        out.push(Line::from(Span::styled(
            format!("  来源：{}", c.how_i_know),
            dim(),
        )));
    }
    if !card.competitors_note.trim().is_empty() {
        out.push(Line::from(Span::styled(
            card.competitors_note.clone(),
            dim(),
        )));
    }
    out.push(Line::default());

    // ---- 坑
    out.push(Line::from(Span::styled(
        "── 这是个坑 ",
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    )));
    out.extend(text_lines(&card.trap));
    out.push(Line::default());

    if !card.insufficient_evidence.is_empty() {
        out.push(Line::from(Span::styled(
            format!("证据不足而留空：{}", card.insufficient_evidence.join("、")),
            dim(),
        )));
        out.push(Line::default());
    }

    // ---- 元信息：质量突然波动时第一个要看的是 provider
    let mut meta = format!(
        "analysis #{} · prompt {} · {}{} · {}",
        analysis.id,
        analysis.prompt_version,
        analysis.model,
        analysis
            .provider
            .as_ref()
            .map(|p| format!(" via {p}"))
            .unwrap_or_default(),
        analysis
            .created_at
            .get(..16)
            .unwrap_or(&analysis.created_at)
    );
    if let (Some(i), Some(o)) = (analysis.tokens_in, analysis.tokens_out) {
        meta.push_str(&format!(" · {i}→{o} tokens"));
    }
    if let Some(c) = analysis.cost_usd {
        meta.push_str(&format!(" · ${c:.4}"));
    }
    out.push(Line::from(Span::styled(meta, dim())));
    out
}

fn push_evidence(out: &mut Vec<Line<'static>>, evidence: &[Evidence]) {
    let bar = Style::new().fg(Color::Blue);
    for e in evidence {
        for l in e.quote.trim().lines() {
            out.push(Line::from(vec![
                Span::styled("│ ", bar),
                Span::styled(l.to_string(), Style::new().add_modifier(Modifier::ITALIC)),
            ]));
        }
        out.push(Line::from(vec![
            Span::styled("│ ", bar),
            Span::styled(format!("— {}", e.source_ref), dim()),
        ]));
    }
}
