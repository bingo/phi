//! 领域模型。
//!
//! `OpportunityCard` 同时是两件事的唯一真相来源：喂给模型的 JSON Schema，
//! 和反序列化的目标类型。改字段时不会出现「schema 改了但 struct 忘了改」这种
//! 只在运行期炸的问题。

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- 采集侧模型

/// 归一化条目：任何来源的一条「产品 / 新闻」都落成这个形状。
/// 源站特有字段进 `raw`，不进主表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: i64,
    pub source: String,
    pub source_id: String,
    pub slug: Option<String>,
    pub url: String,
    pub name: String,
    pub tagline: Option<String>,
    pub description: Option<String>,
    pub website: Option<String>,
    pub posted_at: Option<String>,
    /// 归一化的讨论热度。ProductHunt 上就是 commentsCount。
    pub signal_count: i64,
    pub vote_count: i64,
    pub topics: Vec<String>,
    pub fetched_at: String,
    pub comments_fetched_at: Option<String>,
}

/// 尚未入库的条目，由各 `Source` 归一化后产出。
#[derive(Debug, Clone)]
pub struct NewItem {
    pub source: String,
    pub source_id: String,
    pub slug: Option<String>,
    pub url: String,
    pub name: String,
    pub tagline: Option<String>,
    pub description: Option<String>,
    pub website: Option<String>,
    pub posted_at: Option<String>,
    pub signal_count: i64,
    pub vote_count: i64,
    pub topics: Vec<String>,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: i64,
    pub item_id: i64,
    pub source_comment_id: String,
    pub author: Option<String>,
    pub is_maker: bool,
    pub body: String,
    pub votes: i64,
    pub created_at: Option<String>,
    /// 预过滤结果。`false` 表示被判定为寒暄噪音 —— 只打标，不删除，
    /// 这样改了规则重跑过滤即可，不必再花 PH 配额重抓。
    pub kept: bool,
    pub filter_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewComment {
    pub source_comment_id: String,
    pub author: Option<String>,
    pub is_maker: bool,
    pub body: String,
    pub votes: i64,
    pub created_at: Option<String>,
}

/// 定位一条源站记录。多数源站用 slug，有些用数字 id。
#[derive(Debug, Clone)]
pub enum ItemKey {
    Slug(String),
    SourceId(String),
}

/// 源站列表查询的**一页**的条件。翻页、切片、续传都在 `usecase::sync` 里做。
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub topic: Option<String>,
    pub posted_after: Option<DateTime<Utc>>,
    pub posted_before: Option<DateTime<Utc>>,
}

/// `phi sync` 的参数。日期都是 UTC 自然日，闭区间。
#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub topic: Option<String>,
    pub since: chrono::NaiveDate,
    pub until: chrono::NaiveDate,
    /// 本次最多发多少个请求（每个请求 = 一页）。用完就停，进度已存，下次续传
    pub max_pages: Option<usize>,
    /// 忽略已完成标记和已存游标，全部从头拉 —— 用来刷新旧日期的票数 / 评论数
    pub refresh: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// 请求的日期范围一共几天
    pub days: usize,
    /// 之前已经完整拉过、这次跳过的天数
    pub days_skipped: usize,
    /// 这次翻完最后一页并标记完成的天数
    pub days_completed: usize,
    /// 这次拉完了、但当天还没结束所以不标记完成的天数
    pub days_open: usize,
    pub pages: usize,
    pub inserted: usize,
    pub updated: usize,
    /// 是否因为 `max_pages` 用完而提前停下
    pub budget_exhausted: bool,
}

impl SyncReport {
    /// 没拉完的天数（预算用完时，正在拉的那天和后面没轮到的）
    pub fn days_pending(&self) -> usize {
        self.days - self.days_skipped - self.days_completed - self.days_open
    }
}

#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------- 机会卡片

/// 三态判断。刻意不用数字 —— 模型对 0-100 的校准很差，会扁平地全给 65-80，
/// 制造出一种其实不存在的精度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    Yes,
    Unsure,
    No,
}

impl Tri {
    pub fn parse(s: &str) -> Option<Tri> {
        match s {
            "yes" => Some(Tri::Yes),
            "unsure" => Some(Tri::Unsure),
            "no" => Some(Tri::No),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Tri::Yes => "yes",
            Tri::Unsure => "unsure",
            Tri::No => "no",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Follow,
    Watch,
    Drop,
}

impl Verdict {
    pub fn parse(s: &str) -> Option<Verdict> {
        match s {
            "follow" => Some(Verdict::Follow),
            "watch" => Some(Verdict::Watch),
            "drop" => Some(Verdict::Drop),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Follow => "follow",
            Verdict::Watch => "watch",
            Verdict::Drop => "drop",
        }
    }
}

/// 一个轴的判断 + 一句具体理由。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Axis {
    pub value: Tri,
    /// 一句具体理由。不要写「需要进一步评估」这类无信息量的句子。
    pub reason: String,
}

/// 从原文逐字摘出的证据。**不翻译** —— 翻译会让读者无法回到原页面核验。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Evidence {
    /// 原文逐字摘录。MUST NOT be translated — keep the source language verbatim.
    pub quote: String,
    /// 来源标识：评论 id、`description`、或页面 URL。
    pub source_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Competitor {
    pub name: String,
    /// 和本产品的差异。
    pub difference: String,
    /// 「我知道它是因为……」—— 针对幻觉竞品名的缓解。想不出来源就别列。
    pub how_i_know: String,
}

/// 机会卡片。这是整个工具的产出物。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpportunityCard {
    /// 去营销化的一句话：它替谁解决什么。不要复述 tagline。
    pub one_liner: String,

    /// 一人或 2-3 人团队 3 个月内能不能做出可用版本。
    pub buildable: Axis,
    /// 做出来有没有足够的付费需求。高票数不等于付费意愿。
    pub worth_it: Axis,
    /// 没有预算和销售团队的独立开发者能不能触达这批用户。
    pub reachable: Axis,

    /// 它赌的是哪个真实痛点。没有证据就设为 null。
    pub pain: Option<String>,
    pub pain_evidence: Vec<Evidence>,

    /// 付费方是谁，为什么掏钱。没有证据就设为 null。
    pub who_pays: Option<String>,
    pub who_pays_evidence: Vec<Evidence>,

    /// 复刻门槛：需要什么能力、数据、资质、冷启动网络。
    pub build_cost: String,
    /// 如果照着做，多久能追上。
    pub moat: String,
    /// 它怎么获客，这条渠道独立开发者能不能复用。
    pub distribution: String,
    /// 定价结构与价格锚点。
    pub business_model: String,

    /// 空位：评论区抱怨、没覆盖的细分人群、地域空白。没有证据就设为 null。
    pub gap: Option<String>,
    pub gap_evidence: Vec<Evidence>,

    /// 确实知道的已有竞品。想不出来就返回空数组，不要编。
    pub competitors: Vec<Competitor>,
    /// 竞品列表为空或不完整时说明原因。
    pub competitors_note: String,

    /// 为什么这个方向对一个人或小团队是坑。必填 —— 这一栏用来对冲附和倾向。
    pub trap: String,

    pub verdict: Verdict,
    pub verdict_reason: String,

    /// 因证据不足而留空的字段名列表。
    pub insufficient_evidence: Vec<String>,
}

impl OpportunityCard {
    /// 拍平成一段纯文本，用于 FTS 索引。
    pub fn to_search_text(&self) -> String {
        let mut parts = vec![
            self.one_liner.clone(),
            self.buildable.reason.clone(),
            self.worth_it.reason.clone(),
            self.reachable.reason.clone(),
            self.build_cost.clone(),
            self.moat.clone(),
            self.distribution.clone(),
            self.business_model.clone(),
            self.trap.clone(),
            self.verdict_reason.clone(),
        ];
        parts.extend(self.pain.clone());
        parts.extend(self.who_pays.clone());
        parts.extend(self.gap.clone());
        for c in &self.competitors {
            parts.push(format!("{} {}", c.name, c.difference));
        }
        parts.join("\n")
    }
}

/// 一次分析的完整记录。带快照，所以可以换 prompt 或换模型重跑再 diff。
#[derive(Debug, Clone)]
pub struct Analysis {
    pub id: i64,
    pub item_id: i64,
    pub created_at: String,
    pub prompt_version: String,
    pub model: String,
    pub provider: Option<String>,
    pub card: OpportunityCard,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cost_usd: Option<f64>,
}

/// 模型调用的用量回执。
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cost_usd: Option<f64>,
    /// OpenRouter 实际路由到的 provider。质量突然波动时第一个查它。
    pub provider: Option<String>,
}

// ---------------------------------------------------------------- 浏览视图

/// 列表里的一行：item + 最新一次分析的摘要。TUI 和以后的 server 共用。
#[derive(Debug, Clone)]
pub struct ItemSummary {
    pub item: Item,
    pub latest_analysis_id: Option<i64>,
    pub verdict: Option<Verdict>,
    pub buildable: Option<Tri>,
    pub worth_it: Option<Tri>,
    pub reachable: Option<Tri>,
    pub analysis_count: i64,
    pub note_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnalysisState {
    #[default]
    All,
    Analyzed,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// 评论数降序 —— 评论是用户声音的唯一硬证据源，所以是默认排序
    #[default]
    Comments,
    Votes,
    Newest,
    Name,
}

/// 浏览视图的筛选条件。全部是**视图层**的过滤：阈值和条件选错了不用重拉数据。
#[derive(Debug, Clone, Default)]
pub struct OverviewQuery {
    /// 评论数阈值
    pub min_signal: i64,
    pub state: AnalysisState,
    /// 下面四个按**最新一次**分析筛；设置了任何一个，未分析的条目自然被排除
    pub verdict: Option<Verdict>,
    pub buildable: Option<Tri>,
    pub worth_it: Option<Tri>,
    pub reachable: Option<Tri>,
    /// 子串匹配：名称 / tagline / 描述 / 任一版本的卡片正文 / 笔记
    pub text: Option<String>,
    pub sort: SortKey,
}

#[derive(Debug, Clone)]
pub struct Overview {
    pub rows: Vec<ItemSummary>,
    /// 筛选前的总条数
    pub total: usize,
}

/// 一条 item 的全部内容。分析和笔记是两条独立的线，这里只是把它们并排放在一起。
#[derive(Debug, Clone)]
pub struct ItemDetail {
    pub item: Item,
    /// 新的在前
    pub analyses: Vec<Analysis>,
    /// (created_at, body)，新的在前
    pub notes: Vec<(String, String)>,
}
