//! 把机会卡片渲染成 markdown。
//!
//! 刻意把「证据」和「结论」排在一起 —— 一个没有证据的判断对读者是负资产，
//! 它看起来像信息，但会误导决策。排版让这件事一眼可见。

use crate::model::{Analysis, Comment, Item, OpportunityCard, Tri, Verdict};

pub fn tri_mark(t: Tri) -> &'static str {
    match t {
        Tri::Yes => "[是]",
        Tri::Unsure => "[存疑]",
        Tri::No => "[否]",
    }
}

pub fn verdict_label(v: Verdict) -> &'static str {
    match v {
        Verdict::Follow => "跟进",
        Verdict::Watch => "观望",
        Verdict::Drop => "放弃",
    }
}

fn section(out: &mut String, title: &str, body: &Option<String>, missing_hint: &str) {
    out.push_str(&format!("### {title}\n\n"));
    match body {
        Some(b) if !b.trim().is_empty() => out.push_str(&format!("{b}\n\n")),
        _ => out.push_str(&format!("_{missing_hint}_\n\n")),
    }
}

pub fn card_markdown(item: &Item, analysis: &Analysis) -> String {
    let card = &analysis.card;
    let mut out = String::new();

    out.push_str(&format!("# {}\n\n", item.name));
    out.push_str(&format!("> {}\n\n", card.one_liner));
    out.push_str(&format!(
        "`#{}` · {} 票 · {} 条评论 · {}\n\n",
        item.id,
        item.vote_count,
        item.signal_count,
        item.posted_at.as_deref().unwrap_or("发布时间未知")
    ));
    out.push_str(&format!("{}\n\n", item.url));

    // ---- 三轴
    out.push_str("## 判断\n\n");
    out.push_str("| 轴 | 判断 | 理由 |\n|---|---|---|\n");
    for (label, axis) in [
        ("做得出来吗", &card.buildable),
        ("值得做吗", &card.worth_it),
        ("卖得出去吗", &card.reachable),
    ] {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            label,
            tri_mark(axis.value),
            axis.reason.replace('\n', " ").replace('|', "\\|")
        ));
    }
    out.push_str(&format!(
        "\n**结论：{}** — {}\n\n",
        verdict_label(card.verdict),
        card.verdict_reason
    ));

    // ---- 带证据的三栏
    out.push_str("## 需求\n\n");
    section(&mut out, "痛点", &card.pain, "证据不足，留空");
    push_evidence(&mut out, &card.pain_evidence);
    section(&mut out, "谁付钱", &card.who_pays, "证据不足，留空");
    push_evidence(&mut out, &card.who_pays_evidence);
    section(&mut out, "空位", &card.gap, "证据不足，留空");
    push_evidence(&mut out, &card.gap_evidence);

    // ---- 可行性
    out.push_str("## 可行性\n\n");
    out.push_str(&format!("**复刻门槛**　{}\n\n", card.build_cost));
    out.push_str(&format!("**护城河**　{}\n\n", card.moat));
    out.push_str(&format!("**获客渠道**　{}\n\n", card.distribution));
    out.push_str(&format!("**商业模式**　{}\n\n", card.business_model));

    // ---- 竞品
    out.push_str("## 竞品\n\n");
    if card.competitors.is_empty() {
        out.push_str(&format!(
            "_未列出_ — {}\n\n",
            if card.competitors_note.trim().is_empty() {
                "模型未说明原因"
            } else {
                &card.competitors_note
            }
        ));
    } else {
        for c in &card.competitors {
            out.push_str(&format!(
                "- **{}** — {}\n  <sub>来源：{}</sub>\n",
                c.name, c.difference, c.how_i_know
            ));
        }
        if !card.competitors_note.trim().is_empty() {
            out.push_str(&format!("\n{}\n", card.competitors_note));
        }
        out.push('\n');
    }

    // ---- 坑
    out.push_str("## 这是个坑\n\n");
    out.push_str(&format!("{}\n\n", card.trap));

    if !card.insufficient_evidence.is_empty() {
        out.push_str(&format!(
            "> 证据不足而留空的字段：{}\n\n",
            card.insufficient_evidence.join("、")
        ));
    }

    out.push_str("---\n\n");
    out.push_str(&format!(
        "<sub>analysis #{} · prompt {} · model {}{} · {}",
        analysis.id,
        analysis.prompt_version,
        analysis.model,
        analysis
            .provider
            .as_ref()
            .map(|p| format!(" via {p}"))
            .unwrap_or_default(),
        analysis.created_at
    ));
    if let (Some(i), Some(o)) = (analysis.tokens_in, analysis.tokens_out) {
        out.push_str(&format!(" · {i}→{o} tokens"));
    }
    if let Some(c) = analysis.cost_usd {
        out.push_str(&format!(" · ${c:.4}"));
    }
    out.push_str("</sub>\n");

    out
}

fn push_evidence(out: &mut String, ev: &[crate::model::Evidence]) {
    for e in ev {
        out.push_str(&format!(
            "> {}\n>\n> <sub>— {}</sub>\n\n",
            e.quote.trim(),
            e.source_ref
        ));
    }
}

/// 过滤结果的抽查视图。跑完前 20 个产品后用它看误杀率。
pub fn filter_report(comments: &[Comment]) -> String {
    let total = comments.len();
    let kept = comments.iter().filter(|c| c.kept).count();
    let mut out = format!(
        "评论 {total} 条，保留 {kept} 条，滤掉 {} 条\n\n",
        total - kept
    );

    let mut by_reason: std::collections::BTreeMap<&str, usize> = Default::default();
    for c in comments.iter().filter(|c| !c.kept) {
        *by_reason
            .entry(c.filter_reason.as_deref().unwrap_or("unknown"))
            .or_default() += 1;
    }
    for (reason, n) in by_reason {
        out.push_str(&format!("  {reason:<20} {n}\n"));
    }
    out
}

/// 两个版本的卡片并排对比。换 prompt 或换模型后用它判断改好了还是改坏了。
pub fn diff_cards(a: &Analysis, b: &Analysis) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "A: #{} prompt {} model {}\nB: #{} prompt {} model {}\n\n",
        a.id, a.prompt_version, a.model, b.id, b.prompt_version, b.model
    ));

    let axes = |c: &OpportunityCard| {
        format!(
            "{}/{}/{} → {}",
            c.buildable.value.as_str(),
            c.worth_it.value.as_str(),
            c.reachable.value.as_str(),
            c.verdict.as_str()
        )
    };
    out.push_str(&format!("三轴 A  {}\n", axes(&a.card)));
    out.push_str(&format!("三轴 B  {}\n\n", axes(&b.card)));

    for (label, fa, fb) in [
        ("一句话", &a.card.one_liner, &b.card.one_liner),
        ("坑", &a.card.trap, &b.card.trap),
        ("获客", &a.card.distribution, &b.card.distribution),
    ] {
        out.push_str(&format!("## {label}\n\nA: {fa}\n\nB: {fb}\n\n"));
    }

    let ev = |c: &OpportunityCard| {
        c.pain_evidence.len() + c.who_pays_evidence.len() + c.gap_evidence.len()
    };
    out.push_str(&format!(
        "证据条数  A={}  B={}\n留空字段  A={:?}  B={:?}\n",
        ev(&a.card),
        ev(&b.card),
        a.card.insufficient_evidence,
        b.card.insufficient_evidence
    ));
    out
}
