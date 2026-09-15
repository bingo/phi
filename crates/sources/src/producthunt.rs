//! ProductHunt GraphQL v2 适配器。
//!
//! # 配额
//!
//! 有两道**互相独立**的限制（2026-09-15 实测，不是照文档写的）：
//!
//! 1. **窗口配额**：6250 points / 15 分钟。实测每个请求固定扣 100，和查询复杂度无关，
//!    **被拒的查询也照扣**。响应头 `X-Rate-Limit-Remaining` / `X-Rate-Limit-Reset`，
//!    剩余值反映的是本次请求扣费之前的状态。
//! 2. **单查询复杂度上限**：500,000，发请求前静态计算，超了直接报 GraphQL error。
//!    嵌套连接相乘，而且标量字段很贵。实测拟合（`comments(first:T){ … replies(first:R) }`）：
//!    每个顶层评论槽 ≈ 280，每个回复槽 ≈ 3600（带 `user{}`）/ ≈ 2000（不带）。
//!
//! 所以 `posts(first:50){ comments(first:30) }` 这种一把梭的查询根本发不出去，
//! 而且拉回来的大部分是你根本不会分析的产品的评论。采集因此拆成两阶段：
//! `list()` 只取轻量元数据（含 commentsCount），`fetch_discussion()` 只对候选做。
//!
//! # 认证
//!
//! 用 Developer Token（API dashboard 里生成，不过期）。自用工具不需要 OAuth 流程。
//!
//! # 字段名
//!
//! 下面的 GraphQL 查询串是手写的（只有三个 query，codegen 的维护成本大于收益）。
//! 如果 PH 改了 schema，错误会以 GraphQL error 的形式出现 —— `gql()` 会把完整的
//! 错误响应打出来，照着改这个文件里的查询串即可。

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;
use tracing::{debug, info, warn};

use phi_core::config::ProductHuntConfig;
use phi_core::model::{ItemKey, ListQuery, NewComment, NewItem, Page};
use phi_core::ports::Source;

const SOURCE_ID: &str = "producthunt";

pub struct ProductHunt {
    http: reqwest::Client,
    cfg: ProductHuntConfig,
    token: String,
    rate: Mutex<RateState>,
}

#[derive(Debug, Default, Clone, Copy)]
struct RateState {
    remaining: Option<i64>,
    reset_in: Option<u64>,
}

impl ProductHunt {
    pub fn new(cfg: &ProductHuntConfig, token: String) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("phi/0.1 (+personal research tool)")
                .timeout(Duration::from_secs(45))
                .build()?,
            cfg: cfg.clone(),
            token,
            rate: Mutex::new(RateState::default()),
        })
    }

    /// 剩余复杂度点数低于阈值时，睡到配额窗口重置。
    async fn respect_budget(&self) {
        let state = *self.rate.lock().unwrap();
        let (Some(remaining), Some(reset_in)) = (state.remaining, state.reset_in) else {
            return;
        };
        if remaining >= self.cfg.complexity_floor {
            return;
        }
        // +2s 余量，免得刚好卡在边界上又被拒。
        let wait = reset_in.saturating_add(2).min(16 * 60);
        warn!(
            remaining,
            floor = self.cfg.complexity_floor,
            wait_secs = wait,
            "复杂度配额不足，等待窗口重置"
        );
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let mut s = self.rate.lock().unwrap();
        s.remaining = None;
        s.reset_in = None;
    }

    async fn gql(&self, query: &str, variables: Value) -> Result<Value> {
        self.respect_budget().await;

        let resp = self
            .http
            .post(&self.cfg.api_url)
            .bearer_auth(&self.token)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .context("请求 ProductHunt API 失败")?;

        let status = resp.status();
        let header = |name: &str| -> Option<i64> {
            resp.headers()
                .get(name)?
                .to_str()
                .ok()?
                .trim()
                .parse::<i64>()
                .ok()
        };
        let remaining = header("x-rate-limit-remaining");
        let reset = header("x-rate-limit-reset");
        {
            let mut s = self.rate.lock().unwrap();
            s.remaining = remaining;
            s.reset_in = reset.map(|r| r.max(0) as u64);
        }
        debug!(?remaining, ?reset, "PH 配额");

        let body = resp.text().await.context("读取 ProductHunt 响应失败")?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            bail!(
                "ProductHunt 返回 429（复杂度配额耗尽）。剩余 {:?}，{:?} 秒后重置。",
                remaining,
                reset
            );
        }
        if !status.is_success() {
            bail!("ProductHunt 返回 {status}: {body}");
        }

        let v: Value = serde_json::from_str(&body)
            .with_context(|| format!("ProductHunt 响应不是合法 JSON: {body}"))?;

        // GraphQL 的错误是 200 + errors 字段。整段打出来，方便对着改上面的查询串。
        if let Some(errors) = v.get("errors") {
            bail!(
                "ProductHunt GraphQL 报错。多半是字段名对不上了 —— \
                 照着错误信息改 crates/sources/src/producthunt.rs 里的查询串。\n{}",
                serde_json::to_string_pretty(errors).unwrap_or_else(|_| errors.to_string())
            );
        }

        v.get("data")
            .cloned()
            .ok_or_else(|| anyhow!("ProductHunt 响应缺少 data 字段: {body}"))
    }
}

// ---------------------------------------------------------------- 响应类型

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PhPost {
    id: String,
    name: String,
    slug: Option<String>,
    tagline: Option<String>,
    description: Option<String>,
    url: Option<String>,
    website: Option<String>,
    votes_count: Option<i64>,
    comments_count: Option<i64>,
    created_at: Option<String>,
    #[serde(default)]
    topics: Option<Connection<NamedNode>>,
    // makers 也在查询里要了，但只在 fetch_discussion 里用原始 JSON 读 ——
    // 那里需要的是 id 集合，用来判定某条评论是不是作者本人发的。
}

#[derive(Debug, Deserialize)]
struct PhUser {
    id: String,
    name: Option<String>,
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedNode {
    name: String,
}

// camelCase 不能省：少了它 `pageInfo` 永远反序列化成 None，所有翻页都会静默地只抓第一页。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    #[serde(default = "Vec::new")]
    edges: Vec<Edge<T>>,
    #[serde(default)]
    page_info: Option<PageInfo>,
}

#[derive(Debug, Deserialize)]
struct Edge<T> {
    node: T,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PhComment {
    id: String,
    body: Option<String>,
    votes_count: Option<i64>,
    created_at: Option<String>,
    #[serde(default)]
    user: Option<PhUser>,
    /// 只有顶层评论的查询里会要这个字段。
    #[serde(default)]
    replies: Option<Connection<PhComment>>,
}

// ---------------------------------------------------------------- 查询串

const POST_FIELDS: &str = r#"
    id
    name
    slug
    tagline
    description
    url
    website
    votesCount
    commentsCount
    createdAt
    topics(first: 10) { edges { node { name } } }
"#;

fn post_query() -> String {
    format!(
        r#"query PhiPost($slug: String!) {{
            post(slug: $slug) {{
                {POST_FIELDS}
                makers {{ id name username }}
            }}
        }}"#
    )
}

fn post_by_id_query() -> String {
    format!(
        r#"query PhiPostById($id: ID!) {{
            post(id: $id) {{
                {POST_FIELDS}
                makers {{ id name username }}
            }}
        }}"#
    )
}

/// 阶段一的批量查询。
///
/// `topic` / `postedAfter` / `postedBefore` 内联进查询串而不是声明成 GraphQL 变量 ——
/// 这几个参数的 scalar 类型名不好确定，内联成字面量可以避开声明，出错时也更好改。
/// `order` 刻意不传：枚举值名不确定，默认排序够用。
fn posts_query(q: &ListQuery) -> String {
    let mut args = vec!["first: 50".to_string(), "after: $after".to_string()];
    if let Some(t) = &q.topic {
        args.push(format!("topic: {}", json!(t)));
    }
    if let Some(a) = &q.posted_after {
        args.push(format!("postedAfter: {}", json!(a.to_rfc3339())));
    }
    if let Some(b) = &q.posted_before {
        args.push(format!("postedBefore: {}", json!(b.to_rfc3339())));
    }
    let args = args.join(", ");

    format!(
        r#"query PhiPosts($after: String) {{
            posts({args}) {{
                pageInfo {{ hasNextPage endCursor }}
                edges {{ node {{ {POST_FIELDS} }} }}
            }}
        }}"#
    )
}

/// 评论查询每页的顶层评论数。和 [`INLINE_REPLIES`] 的乘积受单查询复杂度上限约束：
/// 20 × 10 × ~2000 + 20 × ~280 ≈ 406k < 500k。要调大先重新实测，别凭感觉。
const COMMENT_PAGE_SIZE: u32 = 20;
/// 每条顶层评论内联抓的回复数。超出的由 [`replies_query`] 单独补抓。
/// 回复里**不要** `user {}`：它会让单槽成本从 ~2000 涨到 ~3600，
/// 而且 PH 对第三方应用返回的评论作者本来就是 `[REDACTED]`。
const INLINE_REPLIES: u32 = 10;
/// 顶层评论最多翻几页。保险丝：评论上千的爆款不至于把配额吸干（每页 100 点）。
const MAX_COMMENT_PAGES: u32 = 10;
/// 回复超过 [`INLINE_REPLIES`] 时的补抓请求数上限。每个请求同样扣 100 点。
const MAX_REPLY_REQUESTS: u32 = 10;

fn comments_query(by_slug: bool) -> String {
    let (decl, selector) = if by_slug {
        ("$slug: String!, $after: String", "post(slug: $slug)")
    } else {
        ("$id: ID!, $after: String", "post(id: $id)")
    };
    format!(
        r#"query PhiComments({decl}) {{
            {selector} {{
                id
                makers {{ id name username }}
                comments(first: {COMMENT_PAGE_SIZE}, after: $after) {{
                    pageInfo {{ hasNextPage endCursor }}
                    edges {{ node {{
                        id body votesCount createdAt user {{ id name username }}
                        replies(first: {INLINE_REPLIES}) {{
                            pageInfo {{ hasNextPage endCursor }}
                            edges {{ node {{ {REPLY_FIELDS} }} }}
                        }}
                    }} }}
                }}
            }}
        }}"#
    )
}

const REPLY_FIELDS: &str = "id body votesCount createdAt";

/// 某条顶层评论的回复超过 [`INLINE_REPLIES`] 时，从内联那页的 endCursor 接着翻。
fn replies_query() -> String {
    format!(
        r#"query PhiReplies($id: ID!, $after: String) {{
            comment(id: $id) {{
                replies(first: 50, after: $after) {{
                    pageInfo {{ hasNextPage endCursor }}
                    edges {{ node {{ {REPLY_FIELDS} }} }}
                }}
            }}
        }}"#
    )
}

// ---------------------------------------------------------------- 归一化

fn to_new_comment(c: PhComment, maker_ids: &[String]) -> Option<NewComment> {
    let body = html_to_text(&c.body?);
    let user_id = c.user.as_ref().map(|u| u.id.clone());
    let author = c
        .user
        .as_ref()
        .and_then(|u| u.username.clone().or_else(|| u.name.clone()));
    Some(NewComment {
        source_comment_id: c.id,
        author,
        is_maker: user_id.map(|id| maker_ids.contains(&id)).unwrap_or(false),
        body,
        votes: c.votes_count.unwrap_or(0),
        created_at: c.created_at,
    })
}

/// PH 的评论 body 是 HTML（`<p>`、`<br>`、`<a>`、实体转义）。转成页面上肉眼看到的纯文本：
/// 证据引文要能拿去原页面搜到，`min_length` 也不该把标签算进字数。
///
/// 故意手写而不引依赖：PH 评论里实际只出现很小的一个 HTML 子集。
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    // 当前 <a> 的 href，用来在链接文字和 href 不同时把 href 补在后面
    let mut href: Option<String> = None;
    let mut link_text_start = 0;

    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..lt]));
        let Some(gt) = rest[lt..].find('>') else {
            // 不成对的 '<'，按字面文本处理
            out.push_str(&decode_entities(&rest[lt..]));
            rest = "";
            break;
        };
        let tag = &rest[lt + 1..lt + gt];
        rest = &rest[lt + gt + 1..];

        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        match (name.as_str(), closing) {
            ("br", _) => out.push('\n'),
            (
                "p" | "div" | "ul" | "ol" | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6",
                true,
            ) => out.push_str("\n\n"),
            ("li", false) => out.push_str("\n- "),
            ("a", false) => {
                href = attr(tag, "href").map(|h| decode_entities(&h));
                link_text_start = out.len();
            }
            ("a", true) => {
                if let Some(h) = href.take() {
                    let text = out[link_text_start..].trim();
                    if !h.is_empty() && text != h {
                        out.push_str(&format!(" ({h})"));
                    }
                }
            }
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest));

    // 收拾空白：行尾空格去掉，连续空行压成一行
    let mut cleaned = String::with_capacity(out.len());
    let mut blank_run = 0;
    for line in out.lines().map(str::trim) {
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    cleaned.trim().to_string()
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=");
    let i = tag.find(&pat)? + pat.len();
    let v = &tag[i..];
    let (quote, v) = match v.chars().next()? {
        q @ ('"' | '\'') => (q, &v[1..]),
        _ => (' ', v),
    };
    Some(v.split(quote).next().unwrap_or("").to_string())
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&semi| semi <= 10).and_then(|semi| {
            let ent = &rest[1..semi];
            let ch = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => {
                    let code = if let Some(hex) =
                        ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X"))
                    {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        ent.strip_prefix('#').and_then(|d| d.parse().ok())
                    };
                    code.and_then(char::from_u32)
                }
            };
            ch.map(|c| (c, semi))
        });
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn to_new_item(p: PhPost, raw: Value) -> NewItem {
    let slug = p.slug.clone();
    let url = p.url.clone().unwrap_or_else(|| {
        format!(
            "https://www.producthunt.com/posts/{}",
            slug.clone().unwrap_or_else(|| p.id.clone())
        )
    });
    NewItem {
        source: SOURCE_ID.into(),
        source_id: p.id,
        slug,
        url,
        name: p.name,
        tagline: p.tagline,
        description: p.description,
        website: p.website,
        posted_at: p.created_at,
        // 归一化的讨论热度。这是「值不值得深挖」那道阈值的排序键。
        signal_count: p.comments_count.unwrap_or(0),
        vote_count: p.votes_count.unwrap_or(0),
        topics: p
            .topics
            .map(|c| c.edges.into_iter().map(|e| e.node.name).collect())
            .unwrap_or_default(),
        raw,
    }
}

// ---------------------------------------------------------------- Source impl

#[async_trait]
impl Source for ProductHunt {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn matches_url(&self, url: &str) -> bool {
        url.contains("producthunt.com")
    }

    fn parse_url(&self, url: &str) -> Result<ItemKey> {
        let parsed = url::Url::parse(url).with_context(|| format!("不是合法 URL: {url}"))?;
        if !parsed
            .host_str()
            .map(|h| h.contains("producthunt.com"))
            .unwrap_or(false)
        {
            bail!("不是 ProductHunt 的链接: {url}");
        }
        let segs: Vec<&str> = parsed
            .path_segments()
            .map(|s| s.filter(|x| !x.is_empty()).collect())
            .unwrap_or_default();

        // /posts/<slug> 和 /products/<slug> 都接受
        match segs.as_slice() {
            [kind, slug, ..] if *kind == "posts" || *kind == "products" => {
                Ok(ItemKey::Slug((*slug).to_string()))
            }
            _ => bail!(
                "从 URL 里认不出 slug: {url}\n期望形如 https://www.producthunt.com/posts/<slug>"
            ),
        }
    }

    async fn list(&self, q: &ListQuery, cursor: Option<&str>) -> Result<Page<NewItem>> {
        let data = self
            .gql(&posts_query(q), json!({ "after": cursor }))
            .await?;

        let conn = data
            .get("posts")
            .cloned()
            .ok_or_else(|| anyhow!("响应缺少 posts"))?;
        let raw_edges = conn
            .get("edges")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default();

        let parsed: Connection<PhPost> =
            serde_json::from_value(conn.clone()).context("解析 posts 连接失败")?;

        let items = parsed
            .edges
            .into_iter()
            .zip(raw_edges.iter())
            .map(|(e, raw)| {
                let raw_node = raw.get("node").cloned().unwrap_or_else(|| raw.clone());
                to_new_item(e.node, raw_node)
            })
            .collect();

        let next_cursor =
            parsed
                .page_info
                .and_then(|p| if p.has_next_page { p.end_cursor } else { None });

        Ok(Page { items, next_cursor })
    }

    async fn fetch_one(&self, key: &ItemKey) -> Result<NewItem> {
        let (query, vars) = match key {
            ItemKey::Slug(s) => (post_query(), json!({ "slug": s })),
            ItemKey::SourceId(id) => (post_by_id_query(), json!({ "id": id })),
        };
        let data = self.gql(&query, vars).await?;
        let raw = data
            .get("post")
            .cloned()
            .filter(|v| !v.is_null())
            .ok_or_else(|| anyhow!("ProductHunt 上找不到这个产品（post 为 null）"))?;
        let post: PhPost = serde_json::from_value(raw.clone()).context("解析 post 失败")?;
        Ok(to_new_item(post, raw))
    }

    async fn fetch_discussion(&self, key: &ItemKey) -> Result<Vec<NewComment>> {
        let (query, base_vars) = match key {
            ItemKey::Slug(s) => (comments_query(true), json!({ "slug": s })),
            ItemKey::SourceId(id) => (comments_query(false), json!({ "id": id })),
        };

        let mut out: Vec<NewComment> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut maker_ids: Vec<String> = Vec::new();
        let mut page_no = 0;
        // (顶层评论 id, 内联那页回复的 endCursor)：回复没抓完的线程，翻完顶层再补。
        let mut unfinished_threads: Vec<(String, Option<String>)> = Vec::new();

        loop {
            let mut vars = base_vars.clone();
            vars["after"] = json!(cursor);

            let data = self.gql(&query, vars).await?;
            let post = data
                .get("post")
                .cloned()
                .filter(|v| !v.is_null())
                .ok_or_else(|| anyhow!("ProductHunt 上找不到这个产品（post 为 null）"))?;

            if page_no == 0 {
                // Comment 上没有可靠的 isMaker 字段，所以拿 post.makers 的 id 自己比对。
                if let Some(makers) = post.get("makers") {
                    let makers: Vec<PhUser> =
                        serde_json::from_value(makers.clone()).unwrap_or_default();
                    maker_ids = makers.into_iter().map(|m| m.id).collect();
                }
            }

            let conn = post
                .get("comments")
                .cloned()
                .ok_or_else(|| anyhow!("响应缺少 comments"))?;
            let parsed: Connection<PhComment> =
                serde_json::from_value(conn).context("解析 comments 连接失败")?;

            for e in parsed.edges {
                let mut c = e.node;
                if let Some(replies) = c.replies.take() {
                    if let Some(PageInfo {
                        has_next_page: true,
                        end_cursor,
                    }) = replies.page_info
                    {
                        unfinished_threads.push((c.id.clone(), end_cursor));
                    }
                    out.extend(
                        replies
                            .edges
                            .into_iter()
                            .filter_map(|r| to_new_comment(r.node, &maker_ids)),
                    );
                }
                out.extend(to_new_comment(c, &maker_ids));
            }

            page_no += 1;
            match parsed.page_info {
                Some(p) if p.has_next_page => cursor = p.end_cursor,
                _ => break,
            }
            if page_no >= MAX_COMMENT_PAGES {
                warn!(pages = MAX_COMMENT_PAGES, "顶层评论翻页达到上限，停止翻页");
                break;
            }
        }

        let mut reply_requests = 0;
        'threads: for (comment_id, mut cursor) in unfinished_threads {
            loop {
                if reply_requests >= MAX_REPLY_REQUESTS {
                    warn!(
                        limit = MAX_REPLY_REQUESTS,
                        "回复补抓请求达到上限，剩余回复不再抓取"
                    );
                    break 'threads;
                }
                reply_requests += 1;
                let data = self
                    .gql(
                        &replies_query(),
                        json!({ "id": comment_id, "after": cursor }),
                    )
                    .await?;
                let conn = data
                    .get("comment")
                    .and_then(|c| c.get("replies"))
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!("响应缺少 comment.replies（comment_id={comment_id}）")
                    })?;
                let parsed: Connection<PhComment> =
                    serde_json::from_value(conn).context("解析 replies 连接失败")?;
                out.extend(
                    parsed
                        .edges
                        .into_iter()
                        .filter_map(|r| to_new_comment(r.node, &maker_ids)),
                );
                match parsed.page_info {
                    Some(p) if p.has_next_page => cursor = p.end_cursor,
                    _ => break,
                }
            }
        }

        info!(
            count = out.len(),
            makers = maker_ids.len(),
            reply_requests,
            "评论抓取完成（含回复）"
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ph() -> ProductHunt {
        ProductHunt::new(&ProductHuntConfig::default(), "test-token".into()).unwrap()
    }

    #[test]
    fn parses_post_url() {
        let k = ph()
            .parse_url("https://www.producthunt.com/posts/some-tool")
            .unwrap();
        match k {
            ItemKey::Slug(s) => assert_eq!(s, "some-tool"),
            _ => panic!("expected slug"),
        }
    }

    #[test]
    fn parses_products_url_with_query() {
        let k = ph()
            .parse_url("https://www.producthunt.com/products/some-tool?ref=homepage")
            .unwrap();
        match k {
            ItemKey::Slug(s) => assert_eq!(s, "some-tool"),
            _ => panic!("expected slug"),
        }
    }

    #[test]
    fn rejects_non_ph_url() {
        assert!(ph().parse_url("https://example.com/posts/x").is_err());
    }

    #[test]
    fn html_to_text_matches_real_ph_bodies() {
        assert_eq!(html_to_text("<p>Love it!</p>"), "Love it!");
        assert_eq!(
            html_to_text("<p>Hey Product Hunt 👋 <br><br>I'm Jason</p><p>Second para</p>"),
            "Hey Product Hunt 👋\n\nI'm Jason\n\nSecond para"
        );
        // 链接文字和 href 相同时不重复
        assert_eq!(
            html_to_text(
                r#"<p>Here's a pure &lt;60 sec demo:</p><p><a href="https://youtu.be/x?si=1" target="_blank" rel="nofollow">https://youtu.be/x?si=1</a></p>"#
            ),
            "Here's a pure <60 sec demo:\n\nhttps://youtu.be/x?si=1"
        );
        // 链接文字和 href 不同时把 href 补在后面
        assert_eq!(
            html_to_text(r#"see <a href="https://a.com/?a=1&amp;b=2">the docs</a>"#),
            "see the docs (https://a.com/?a=1&b=2)"
        );
        // 纯文本、不成对的尖括号、未知实体都原样保留
        assert_eq!(
            html_to_text("Congratulations on the launch team!!"),
            "Congratulations on the launch team!!"
        );
        assert_eq!(
            html_to_text("a < b &unknown; &#39;q&#x27;"),
            "a < b &unknown; 'q'"
        );
    }

    #[test]
    fn comments_query_fits_complexity_budget() {
        // 实测拟合的成本模型，见模块文档。改了页大小这个测试会提醒你重新算。
        let cost = COMMENT_PAGE_SIZE * INLINE_REPLIES * 2000 + COMMENT_PAGE_SIZE * 280;
        assert!(cost < 450_000, "估算复杂度 {cost} 太接近 500k 上限");
        let q = comments_query(true);
        assert!(q.contains("replies(first: 10)"));
        assert!(!q.split("replies(").nth(1).unwrap().contains("user"));
    }

    #[test]
    fn connection_reads_page_info() {
        let c: Connection<NamedNode> = serde_json::from_value(json!({
            "pageInfo": { "hasNextPage": true, "endCursor": "abc" },
            "edges": [{ "node": { "name": "x" } }]
        }))
        .unwrap();
        let p = c.page_info.expect("pageInfo 没读到");
        assert!(p.has_next_page);
        assert_eq!(p.end_cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn posts_query_inlines_optional_args() {
        let q = ListQuery {
            topic: Some("artificial-intelligence".into()),
            limit: 10,
            ..Default::default()
        };
        let s = posts_query(&q);
        assert!(s.contains(r#"topic: "artificial-intelligence""#));
        assert!(!s.contains("postedAfter"));
    }
}
