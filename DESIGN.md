# phi — 设计文档 v0.1

> **ph**oduct**h**unt **i**nspiration tool
>
> 自用的 ProductHunt 产品拆解工具。把有真实用户讨论的产品，拆成带证据的「机会卡片」，
> 用来判断某个方向值不值得作为一人公司 / 小团队的产品切入点。
>
> 2026-09-15 · 仓库：`~/Documents/phi`

---

## 0. 一句话定位

**不是**一个 ProductHunt 信息聚合器，**是**一个判断辅助工具。

它唯一的工作是回答三个问题，并且每个答案都必须挂得上来自真实用户的证据：

1. 这个方向我做得出来吗？
2. 做出来值得吗？
3. 我卖得出去吗？

任何不服务于这三个问题的字段，都不该存在于卡片里。

---

## 1. 已确认的决策

| # | 决策点 | 选择 | 关键理由 |
|---|---|---|---|
| 1 | v1 主循环 | 深度解剖单品 | 先验证分析质量，再谈规模 |
| 2 | 北极星产出 | 机会卡片 | thesis（方向假设）建表预留，不做 UI |
| 3 | 采集范围 | 手动 URL + 分类批量拉取 | 两入口共用一套 pipeline |
| 4 | AI 调用层 | 直接调模型 API | 可控、可做结构化输出、易回归 |
| 5 | 评分 | 三轴三态（是/存疑/否） | 避免 AI 对数字的假精确 |
| 6 | 分析对象 | 只聚焦评论/reviews 多的产品 | 评论是用户声音的唯一硬证据源 |
| 7 | 门槛执行 | 入库全量 + 可调阈值视图 | 阈值改了不用重拉数据 |
| 8 | 噪音处理 | 规则预过滤 + Top N | 保信号密度（不再是为省钱） |
| 9 | 笔记与分析 | 两条线完全分离 | 分析可随意重跑，不碰你的笔记 |
| 10 | AI 硬约束 | 原文证据 + 「这是个坑」+ 竞品列表 | 三条全要 |
| 11 | 存储 | SQLite + FTS5 | 单文件、零运维 |
| 12 | Web UI | v1 不做，只留 HTTP API | 聚焦 |
| 13 | 多源扩展 | 归一化实体 + Source trait | v1 只实现 PH adapter |
| 14 | 质量回归 | 快照 + 可重跑对比 | 建表时加列，事后补不上 |
| 15 | 命令名 | `phi` | producthunt inspiration |
| 16 | 卡片语言 | 中文正文 + 英文术语 + 引证不翻译 | 翻译会毁掉证据的可核验性 |
| 17 | 模型 | OpenRouter · `deepseek/deepseek-v4.1-flash` | 成本不再是设计约束 |

---

## 2. ProductHunt API 的硬约束

核实过的事实，这三条直接塑造了采集层的形状：

**配额是复杂度制**——6250 complexity points / 15 分钟，按请求字段计算，嵌套连接会乘起来。
响应头带 `X-Rate-Limit-Limit` / `X-Rate-Limit-Remaining` / `X-Rate-Limit-Reset`。

**认证走 Developer Token**——API dashboard 里可以拿一个不过期的 token，自用工具不需要 OAuth 流程。
默认只读 public scope。

**字段齐全**——`Post` 上有 `commentsCount`、`votesCount`、`comments` 连接；
`posts` 根查询支持 `order` / `topic` / `postedAfter` / `postedBefore` / 游标分页。

### 由此推出：采集必须两阶段

```
阶段一  sync      posts(first:50, …) 只取轻量元数据（含 commentsCount）
                  → 便宜，可大批量，写入 item 表，不碰评论
                                ↓
              [ 你在 TUI/CLI 里按 commentsCount 排序挑选 ]
                                ↓
阶段二  hydrate   post(id){ comments(first:50, order:VOTES) }  一次一个产品
                  → 贵，只对候选做
                                ↓
阶段三  analyze   预过滤评论 → 组 prompt → 结构化输出 → 落库
```

注意：**两阶段的动机是 PH 的复杂度配额，不是模型成本。**模型那边已经便宜到可以忽略，
但 PH 的 6250 points / 15 分钟是硬墙，绕不过去。

---

## 3. 机会卡片 Schema

### 三轴判断（三态 + 一句话理由）

| 轴 | 问题 | 取值 |
|---|---|---|
| `buildable` | 一人或 2–3 人小团队，3 个月内能不能做出可用版本 | `yes` / `unsure` / `no` |
| `worth_it` | 做出来有没有足够的付费需求 | `yes` / `unsure` / `no` |
| `reachable` | 你能不能触达到这批用户 | `yes` / `unsure` / `no` |

三轴都是 `yes` 才是真机会。分开存是为了能筛出「能做但卖不出去」和「好卖但做不了」这两类——它们的后续动作完全不同，前者要找渠道，后者要找合伙人或放弃。

### 卡片正文

| 字段 | 内容 | 硬约束 |
|---|---|---|
| `one_liner` | 去营销化的一句话：它替谁解决什么 | — |
| `pain` | 它赌的是哪个真实痛点 | **必须挂原文证据** |
| `who_pays` | 付费方是谁，为什么掏钱 | **必须挂原文证据** |
| `build_cost` | 复刻门槛：需要什么能力 / 数据 / 资质 / 冷启动网络 | — |
| `moat` | 如果你抄，多久追上 | — |
| `distribution` | 它怎么获客，这条渠道你能不能复用 | — |
| `business_model` | 定价结构与价格锚点 | — |
| `gap` | 空位：评论区抱怨、没覆盖的细分人群、地域空白 | **必须挂原文证据** |
| `competitors` | 2–3 个已有竞品 + 差异说明 + 认知来源 | **必填，见下** |
| `trap` | 「为什么这个方向对一个人是坑」 | **必填** |
| `verdict` | `follow` / `watch` / `drop` + 理由 | — |

`distribution` 这一栏是刻意加的。OPC 做不成产品，大多数时候不是做不出来，是卖不出去——但几乎所有产品分析模板都不写这一栏。

`trap` 是对冲模型的附和倾向。不强制它写反对意见，它会把一切都说成机会。

`competitors` 允许返回空数组或 `unknown`，**不许硬凑三个**。每个竞品必须附一句
「我知道它是因为……」。这是针对幻觉竞品名的缓解——模型对小众赛道的竞品认知很不可靠。

### 语言规则

- 正文中文。
- 产品名、技术术语、平台名保留英文原文（Notion、RAG、Slack app、churn 不译）。
- `evidence.quote` **一律保留原文，不翻译**。翻译会毁掉可核验性——你回头想验证一条结论时，
  得能拿这句话去原页面里搜到。
- `verdict` / 三轴取值用固定英文枚举，便于程序筛选。

### 语言规则写进 schema，不只写进 prompt

`evidence.quote` 加 `description: "Verbatim quote from the source. MUST NOT be translated."`。
放 schema 里比放 system prompt 里更不容易被模型忘掉。

---

## 4. 数据模型

```sql
-- 归一化条目：任何来源的一条「产品/新闻」都落成这个形状
CREATE TABLE item (
  id                 INTEGER PRIMARY KEY,
  source             TEXT    NOT NULL,          -- 'producthunt' | 'techcrunch' | ...
  source_id          TEXT    NOT NULL,          -- 源站内唯一 id
  url                TEXT    NOT NULL,
  name               TEXT    NOT NULL,
  tagline            TEXT,
  description        TEXT,
  website            TEXT,
  posted_at          TEXT,                      -- ISO8601
  signal_count       INTEGER NOT NULL DEFAULT 0, -- 归一化的「讨论热度」= PH 的 commentsCount
  vote_count         INTEGER NOT NULL DEFAULT 0,
  topics             TEXT,                      -- JSON 数组
  raw                TEXT    NOT NULL,          -- 源站原始响应，保底
  fetched_at         TEXT    NOT NULL,
  comments_fetched_at TEXT,                     -- NULL = 还没进阶段二
  UNIQUE(source, source_id)
);

-- 评论：过滤结果只打标，不删除
CREATE TABLE comment (
  id                INTEGER PRIMARY KEY,
  item_id           INTEGER NOT NULL REFERENCES item(id),
  source_comment_id TEXT    NOT NULL,
  author            TEXT,
  is_maker          INTEGER NOT NULL DEFAULT 0,
  body              TEXT    NOT NULL,
  votes             INTEGER NOT NULL DEFAULT 0,
  created_at        TEXT,
  kept              INTEGER NOT NULL DEFAULT 1, -- 0 = 被判定为寒暄噪音
  filter_reason     TEXT,                       -- 为什么被滤掉，便于调规则
  UNIQUE(item_id, source_comment_id)
);

-- 分析：多版本，带快照
CREATE TABLE analysis (
  id             INTEGER PRIMARY KEY,
  item_id        INTEGER NOT NULL REFERENCES item(id),
  created_at     TEXT    NOT NULL,
  prompt_version TEXT    NOT NULL,
  model          TEXT    NOT NULL,   -- 完整 slug，如 deepseek/deepseek-v4.1-flash
  provider       TEXT,               -- OpenRouter 实际路由到的 provider
  input_snapshot TEXT    NOT NULL,   -- 实际喂进模型的完整输入，JSON
  card           TEXT    NOT NULL,   -- 机会卡片，JSON
  buildable      TEXT,               -- 冗余出来方便索引筛选
  worth_it       TEXT,
  reachable      TEXT,
  verdict        TEXT,
  usefulness     INTEGER,            -- 预留：你没选一键打分，但空列成本为零
  tokens_in      INTEGER,
  tokens_out     INTEGER,
  cost_usd       REAL
);

-- 证据：拆成表，可以审计「这张卡有几条结论没证据」
CREATE TABLE evidence (
  id          INTEGER PRIMARY KEY,
  analysis_id INTEGER NOT NULL REFERENCES analysis(id),
  field       TEXT    NOT NULL,   -- 'pain' | 'who_pays' | 'gap'
  quote       TEXT    NOT NULL,
  source_ref  TEXT               -- comment.id / 'description' / url
);

-- 你的笔记：独立一条线，AI 永不触碰
CREATE TABLE note (
  id         INTEGER PRIMARY KEY,
  item_id    INTEGER NOT NULL REFERENCES item(id),
  created_at TEXT    NOT NULL,
  body       TEXT    NOT NULL
);

-- 采集游标：随时可中断续传
CREATE TABLE sync_cursor (
  source     TEXT PRIMARY KEY,
  query_key  TEXT NOT NULL,       -- topic + 时间区间的指纹
  cursor     TEXT,
  updated_at TEXT NOT NULL
);

-- 方向假设：v1 建表不建 UI
CREATE TABLE thesis (
  id         INTEGER PRIMARY KEY,
  title      TEXT NOT NULL,
  body       TEXT,
  status     TEXT NOT NULL DEFAULT 'open',
  created_at TEXT NOT NULL
);
CREATE TABLE thesis_link (
  thesis_id INTEGER NOT NULL REFERENCES thesis(id),
  item_id   INTEGER NOT NULL REFERENCES item(id),
  stance    TEXT NOT NULL,        -- 'supports' | 'refutes'
  PRIMARY KEY (thesis_id, item_id)
);

-- 全文检索
CREATE VIRTUAL TABLE item_fts USING fts5(
  name, tagline, description, content='item', content_rowid='id');
CREATE VIRTUAL TABLE note_fts USING fts5(
  body, content='note', content_rowid='id');
CREATE VIRTUAL TABLE card_fts USING fts5(card_text);  -- 卡片正文拍平后写入
```

三个设计点值得单独说：

**`comment.kept` 打标而不删除。** 第一版过滤规则一定不准。打标意味着你改了规则之后重跑过滤就行，不用重新消耗 PH 配额去抓评论。`filter_reason` 让你能直接查「被误杀的都是因为哪条规则」。

**`analysis` 是多行不是单行。** 重跑分析追加新行，旧行保留。配合 `input_snapshot` + `prompt_version` + `model`，你才能做 diff。这几列建表时加是免费的，事后补的话——所有旧卡片永远没有快照，永远无法参与回归。

**`note` 独立表且允许多条。** 「两条线完全分离」落到物理层就是这个：分析可以删了重来，笔记不受影响；笔记是时间序列，能看出你对同一个产品的判断怎么变的。

---

## 5. 采集层

### Source trait

```rust
#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &'static str;

    /// 阶段一：批量拉轻量元数据
    async fn list(&self, q: &ListQuery, cursor: Option<&str>) -> Result<Page<RawItem>>;

    /// 单条：用 URL 或源站 id 拉一条
    async fn fetch_one(&self, key: &ItemKey) -> Result<RawItem>;

    /// 阶段二：拉讨论
    async fn fetch_discussion(&self, key: &ItemKey) -> Result<Vec<RawComment>>;
}
```

`RawItem` → `normalize()` → `Item`。源站特有字段全部进 `raw` 列，不进主表。
v1 只实现 `ProductHuntSource`。

GraphQL client 建议**手写 query 字符串 + serde 反序列化**，不引入 cynic / graphql_client 的 codegen——只有三个 query，codegen 的维护成本大于收益。

### 复杂度预算管理

每次响应读 `X-Rate-Limit-Remaining`，维护一个进程内令牌桶。低于阈值（比如 500）就暂停，把游标写进 `sync_cursor`，打印剩余重置秒数后退出。下次 `phi sync` 自动续传。

阶段二 hydrate 可以小并发（3–5），但必须共用同一个令牌桶。

### 评论预过滤规则

**过滤的目的是信号密度，不是省钱。**模型侧成本已经可以忽略，但把 200 条「Congrats 🎉」
塞进上下文，会稀释模型对真正那几条抱怨的注意力。所以规则保留，但阈值可以放宽。

```toml
[comment_filter]
min_length = 60
top_n = 80                            # 成本不是约束了，从 30 提到 80；设 0 表示不限
keep_all_maker_comments = true        # maker 回复里常有路线图和定价解释
noise_patterns = [
  "(?i)congrat", "(?i)good luck", "(?i)all the best",
  "(?i)looks (great|awesome|cool|amazing)", "(?i)love (this|it)",
]
noise_max_length = 150                # 命中噪音词 且 短于此长度 → 丢
```

顺序：纯 emoji / 纯链接 → 丢；短于 `min_length` → 丢；命中噪音词且短于 `noise_max_length` → 丢；
maker 评论无条件保留；剩下按 `votes` 降序取 `top_n`。

喂给模型时每条带上 `[votes=N]` 和 `[maker]` 标记——票数本身就是信号，
一条被顶到高位的抱怨比一条没人理的抱怨重要得多。

---

## 6. AI 分析层

```rust
#[async_trait]
pub trait Analyzer: Send + Sync {
    async fn analyze(&self, input: &AnalysisInput) -> Result<(OpportunityCard, Usage)>;
}
```

v1 实现 `OpenRouterAnalyzer`——OpenAI 兼容协议，所以可以直接用 `async-openai` 并改 base URL，
不需要专门的 OpenRouter SDK。

### 配置

```toml
[analyzer]
base_url = "https://openrouter.ai/api/v1"
model    = "deepseek/deepseek-v4.1-flash"
api_key_env = "OPENROUTER_API_KEY"
temperature = 0.3
```

模型事实（2026-09-15 核实）：
- 输入 **$0.15 / M**，输出 **$0.60 / M**，上下文 **1,048,576 tokens**
- **支持 structured outputs**（出现在 OpenRouter `supported_parameters=structured_outputs` 的模型列表里）
- 发布于 2026-09-10

按每张卡 80 条评论估算，单卡约 **0.2–0.3 美分**，1000 张卡不到 3 美元。成本不构成任何设计约束。

### 结构化输出：必须加 require_parameters

```json
{
  "model": "deepseek/deepseek-v4.1-flash",
  "response_format": {
    "type": "json_schema",
    "json_schema": { "name": "opportunity_card", "strict": true, "schema": { ... } }
  },
  "provider": { "require_parameters": true }
}
```

**`provider.require_parameters: true` 不是可选的。** OpenRouter 的结构化输出支持是
**provider 级**而非 model 级的——同一个模型路由到不同 provider，能力可能不一样。
不加这个字段，请求可能被路由到不支持 `json_schema` 的 provider。

好消息是失败模式是显式报错，不会静默降级成一坨散文——所以这个坑会响，不会闷声出错。

把 OpenRouter 返回的实际 provider 记进 `analysis.provider`，排查质量波动时用得上。

### Prompt 组装

- **system**：角色定义 + 三轴的精确定义 + 三条硬约束 + 禁止项（没有证据不许写 `pain`）+ 语言规则
- **user**：item 元数据 + `kept=1` 的评论，每条标注 `[votes=N]` / `[maker]`
- **schema**：`evidence` 数组 `minItems: 1`，或该字段为 `null` 并在 `insufficient_evidence` 里列出字段名

prompt 模板放 `prompts/card.v1.md`，文件头带 `version:`，写进 `analysis.prompt_version`。

### 重跑与对比

```
phi reanalyze <id> --prompt v2 --diff
phi reanalyze <id> --model anthropic/claude-... --diff    # 换模型对比
```

从 `input_snapshot` 取原始输入重放（不重新抓取，保证只有 prompt / model 在变），并排 diff 两版卡片。
这是你判断「prompt 改好了还是改坏了」的唯一可靠手段。

---

## 7. Rust Workspace

```
phi/
├── Cargo.toml                    # workspace
├── config.toml
├── prompts/
│   └── card.v1.md
└── crates/
    ├── core/        # 领域模型、SQLite 仓储、FTS、过滤规则、用例层
    ├── sources/     # Source trait + producthunt adapter
    ├── analyzer/    # Analyzer trait + openrouter 实现 + prompt 加载
    ├── cli/         # clap 子命令 + ratatui TUI，产出 `phi` 二进制
    └── server/      # axum，只读 JSON API（v1 薄壳）
```

关键约束：**用例函数写在 `core::usecase`，不写在 `cli`。**

```rust
// core/src/usecase.rs
pub async fn ingest_url(ctx: &Ctx, url: &str) -> Result<AnalysisId>;
pub async fn sync(ctx: &Ctx, q: ListQuery) -> Result<SyncReport>;
pub async fn hydrate(ctx: &Ctx, sel: Selection) -> Result<usize>;
pub async fn analyze(ctx: &Ctx, item_id: ItemId, prompt: &str) -> Result<AnalysisId>;
pub async fn search(ctx: &Ctx, q: &str, f: Filters) -> Result<Vec<CardSummary>>;
```

CLI 和 server 都只是这些函数的薄包装。这是「v1 不做 Web 但以后不返工」的全部秘诀——等你要加 Web 时，server 已经是现成的，前端直接消费 JSON API。

**依赖**：`tokio`、`sqlx`(sqlite, 编译期校验)、`clap`(derive)、`ratatui` + `crossterm`、
`reqwest`、`serde`/`serde_json`、`schemars`（从 Rust 类型生成 JSON Schema，保证 schema 和
反序列化目标永远一致）、`axum`、`tracing`、`figment`、`anyhow`/`thiserror`。

`schemars` 值得单独提：`OpportunityCard` 这个 struct 同时是 JSON Schema 的来源和反序列化的目标，
改字段时不会出现「schema 改了但 struct 忘了改」。

---

## 8. CLI 命令

```bash
phi add <url>                 # 主循环：抓取 → 评论 → 过滤 → 分析 → 打印卡片
phi sync --topic ai --since 2026-09-01 --limit 200    # 阶段一批量入库
phi hydrate --min-comments 15 --limit 20              # 阶段二批量拉评论
phi analyze <id>
phi reanalyze <id> --prompt v2 --diff
phi note <id>                 # 打开 $EDITOR 写笔记
phi ls --min-comments 15 --buildable yes --verdict follow
phi search "<query>"
phi tui
phi serve --port 8080         # 只读 JSON API
```

`phi add <url>` 是整个 v1 的心脏——一条命令走完主循环，其它都是它的拆分或批量版本。

TUI 布局按「两条线分离」来：左列表（可筛可排）、中卡片、右笔记。笔记是独立一栏，不是卡片里的一个字段。

---

## 9. 里程碑

### v0.1 — 只验证分析质量（约 1 周）

范围：`phi add <url>` 一条命令。PH 单品抓取 + 评论 + 预过滤 + 分析 + 落库（含快照），卡片以 markdown 打到 stdout。

**不做**：TUI、sync、server、search。

**验收门（重要）**：挑 10 个你本来就熟的产品跑一遍，对每张卡问一句——*这张卡有没有告诉我一件我原本不知道、并且会影响我判断的事？*

如果 7 张以上是正确的废话，**停在这里改 prompt，不要往下做**。后面所有功能都是在放大 v0.1 的输出质量；质量是负的时候，放大只会让它更难看。

**归因动作**：v0.1 就把 `--model` 做成可覆盖的参数。验收门没过时，拿同样 10 个产品跑一次更强的模型
（走同一个 OpenRouter key，改个 slug 就行，成本几毛钱）。这一步决定你接下来该改 prompt 还是该换模型——
没有这个对照，你会在 prompt 上白耗好几天。

### v0.2 — 存量与检索
`phi sync` / `phi hydrate` + 令牌桶 + 游标续传；`phi ls` / `phi search`（FTS5）；`phi note`。

### v0.3 — TUI
ratatui 三栏；`phi reanalyze --diff`。

### v0.4 — HTTP API
axum 只读端点，复用 `core::usecase`。

### 以后
thesis 的 UI、第二个 Source、sqlite-vec 语义检索。

---

## 10. 风险

**分析质量是唯一的真风险。** 采集、存储、TUI 都是确定性工程，写多久就是多久。只有「AI 能不能产出非废话」是不确定的，而且它决定这个工具的死活。所以 v0.1 的验收门不是形式主义。

**模型很新，质量未知。** `deepseek-v4.1-flash` 发布于 2026-09-10，到今天才五天。
它在「基于零散用户评论做商业判断」这类任务上的表现没有可参考的经验。这不是反对用它——
便宜且支持结构化输出，作为默认值很合理——但它把 v0.1 验收门的归因问题放大了：
卡片不行的时候，你分不清是 prompt 烂还是模型弱。上面的「归因动作」就是为这个准备的。

**provider 路由漂移。** 同一个 model slug 在不同时间可能落到不同 provider，输出风格会变。
`analysis.provider` 记下来，质量突然波动时第一个查它。

**竞品幻觉。** 模型对小众赛道的竞品认知不可靠，会编出听起来很真的名字。
缓解已经写进 schema：允许 `unknown`、不许硬凑三个、每个竞品附「我知道它是因为……」。

**评论过滤第一版一定不准。** 所以 `kept` 打标不删除。跑完前 20 个产品后，抽查一遍 `kept=0` 的评论，看误杀率。

**PH 复杂度成本未实测。** `comments(first:50)` 到底吃多少 points 得跑了才知道。第一次跑务必把 rate-limit header 全量打日志，摸清单次成本再定批量大小。
