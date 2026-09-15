//! 评论预过滤。
//!
//! 目的是**信号密度，不是省钱**。模型侧成本已经可以忽略（单卡约 0.2-0.3 美分），
//! 但把 200 条 "Congrats on the launch! 🎉" 塞进上下文，会稀释模型对真正抱怨
//! 那几条的注意力 —— 而那几条正是 `pain` 和 `gap` 两栏的全部来源。
//!
//! 被滤掉的评论**只打标不删除**（`comment.kept = 0` + `filter_reason`）。
//! 第一版规则一定不准；打标意味着改了规则重跑过滤即可，不必再花 PH 配额重抓。

use anyhow::Result;
use regex::RegexSet;

use crate::config::CommentFilterConfig;

/// 过滤器判定的最小输入。让它对 `NewComment` 和已入库的 `Comment` 都能用。
pub trait Filterable {
    fn body(&self) -> &str;
    fn votes(&self) -> i64;
    fn is_maker(&self) -> bool;
}

impl Filterable for crate::model::NewComment {
    fn body(&self) -> &str {
        &self.body
    }
    fn votes(&self) -> i64 {
        self.votes
    }
    fn is_maker(&self) -> bool {
        self.is_maker
    }
}

impl Filterable for crate::model::Comment {
    fn body(&self) -> &str {
        &self.body
    }
    fn votes(&self) -> i64 {
        self.votes
    }
    fn is_maker(&self) -> bool {
        self.is_maker
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub kept: bool,
    /// 为什么被滤掉。留着是为了能直接查「被误杀的都是因为哪条规则」。
    pub reason: Option<String>,
}

impl Decision {
    fn keep() -> Self {
        Self {
            kept: true,
            reason: None,
        }
    }
    fn drop(reason: impl Into<String>) -> Self {
        Self {
            kept: false,
            reason: Some(reason.into()),
        }
    }
}

pub struct CommentFilter {
    cfg: CommentFilterConfig,
    noise: RegexSet,
}

impl CommentFilter {
    pub fn new(cfg: &CommentFilterConfig) -> Result<Self> {
        let noise = RegexSet::new(&cfg.noise_patterns)?;
        Ok(Self {
            cfg: cfg.clone(),
            noise,
        })
    }

    /// 返回与输入等长、顺序一致的判定结果。
    pub fn apply<T: Filterable>(&self, comments: &[T]) -> Vec<Decision> {
        let mut decisions: Vec<Decision> = comments
            .iter()
            .map(|c| self.judge_one(c))
            .collect();

        // top_n 截断：在通过前置规则的那批里按票数降序取前 N。
        if self.cfg.top_n > 0 {
            let mut survivors: Vec<usize> = decisions
                .iter()
                .enumerate()
                .filter(|(_, d)| d.kept)
                .map(|(i, _)| i)
                .collect();

            if survivors.len() > self.cfg.top_n {
                // maker 评论排在前面，其余按票数降序 —— 保证截断先砍掉低票的普通评论。
                survivors.sort_by(|&a, &b| {
                    let (ca, cb) = (&comments[a], &comments[b]);
                    cb.is_maker()
                        .cmp(&ca.is_maker())
                        .then(cb.votes().cmp(&ca.votes()))
                });
                for &idx in &survivors[self.cfg.top_n..] {
                    decisions[idx] = Decision::drop("beyond_top_n");
                }
            }
        }

        decisions
    }

    fn judge_one<T: Filterable>(&self, c: &T) -> Decision {
        // maker 的回复里常有路线图、定价解释和对质疑的回应 —— 信息价值高，无条件保留。
        if self.cfg.keep_all_maker_comments && c.is_maker() {
            return Decision::keep();
        }

        let body = c.body().trim();

        if body.is_empty() {
            return Decision::drop("empty");
        }
        if is_only_links(body) {
            return Decision::drop("link_only");
        }
        if !body.chars().any(|ch| ch.is_alphanumeric()) {
            return Decision::drop("no_text"); // 纯 emoji / 纯标点
        }
        if body.chars().count() < self.cfg.min_length {
            return Decision::drop("too_short");
        }
        if body.chars().count() < self.cfg.noise_max_length && self.noise.is_match(body) {
            let which = self
                .noise
                .matches(body)
                .into_iter()
                .next()
                .map(|i| format!("noise_pattern[{i}]"))
                .unwrap_or_else(|| "noise_pattern".into());
            return Decision::drop(which);
        }

        Decision::keep()
    }
}

fn is_only_links(s: &str) -> bool {
    let non_link: String = s
        .split_whitespace()
        .filter(|w| !(w.starts_with("http://") || w.starts_with("https://")))
        .collect::<Vec<_>>()
        .join("");
    !non_link.chars().any(|c| c.is_alphanumeric())
}

/// 把评论渲染成喂给模型的文本块。
///
/// 票数是信号：一条被顶到高位的抱怨，比一条没人理的抱怨重要得多，所以标出来。
pub fn render_for_prompt<T: Filterable>(comments: &[T]) -> String {
    let mut rows: Vec<&T> = comments.iter().collect();
    rows.sort_by(|a, b| {
        b.is_maker()
            .cmp(&a.is_maker())
            .then(b.votes().cmp(&a.votes()))
    });

    rows.iter()
        .map(|c| {
            let maker = if c.is_maker() { " [maker]" } else { "" };
            format!("- [votes={}]{} {}", c.votes(), maker, c.body().trim())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NewComment;

    fn c(body: &str, votes: i64, is_maker: bool) -> NewComment {
        NewComment {
            source_comment_id: body.chars().take(8).collect(),
            author: None,
            is_maker,
            body: body.into(),
            votes,
            created_at: None,
        }
    }

    fn filter() -> CommentFilter {
        CommentFilter::new(&CommentFilterConfig::default()).unwrap()
    }

    #[test]
    fn drops_congratulations() {
        let input = vec![c("Congrats on the launch! 🎉 Looks awesome.", 3, false)];
        let d = filter().apply(&input);
        assert!(!d[0].kept);
    }

    #[test]
    fn drops_pure_emoji_and_short() {
        let input = vec![c("🎉🎉🎉", 1, false), c("nice", 0, false)];
        let d = filter().apply(&input);
        assert!(!d[0].kept);
        assert!(!d[1].kept);
    }

    #[test]
    fn keeps_substantive_complaint() {
        let body = "I tried this for two weeks. The export is the dealbreaker — \
                    there is no way to get your data out except copy-paste, and for \
                    a team of 12 that is simply not workable.";
        let d = filter().apply(&[c(body, 9, false)]);
        assert!(d[0].kept, "reason: {:?}", d[0].reason);
    }

    #[test]
    fn keeps_short_maker_reply() {
        // maker 的短回复也留 —— 里面常有定价和路线图
        let d = filter().apply(&[c("Pricing is $9/mo, team plan lands in Q4.", 0, true)]);
        assert!(d[0].kept);
    }

    #[test]
    fn top_n_keeps_highest_votes() {
        let cfg = CommentFilterConfig {
            top_n: 2,
            ..Default::default()
        };
        let f = CommentFilter::new(&cfg).unwrap();
        let long = "This is a long enough comment body to survive the length rule \
                    and it says something specific about the workflow.";
        let input = vec![
            c(&format!("{long} A"), 1, false),
            c(&format!("{long} B"), 50, false),
            c(&format!("{long} C"), 20, false),
        ];
        let d = f.apply(&input);
        assert!(!d[0].kept);
        assert_eq!(d[0].reason.as_deref(), Some("beyond_top_n"));
        assert!(d[1].kept);
        assert!(d[2].kept);
    }
}
