# HANDOFF

写给一个完全没参与前期讨论的开发者。读完这一份就能接手。

- **DESIGN.md** —— 决策的完整推导过程，想知道「为什么不是另一种做法」时看它
- **README.md** —— 怎么跑起来、命令速查
- **本文** —— 现在是什么状态、接下来做什么、哪里有雷

最后更新：2026-09-15 · 代码状态：v0.1 功能完整，未经真实 API 调用验证

---

## 1. 这是什么

一个**自用**的命令行工具。输入一个 ProductHunt 产品页 URL，输出一张「机会卡片」——
一份结构化的、带原文证据的拆解，用来判断这个方向值不值得作为一人公司 / 小团队的产品切入点。

它**不是**信息聚合器，不是 newsletter，不是产品数据库。它只回答三个问题：

| 轴 | 问题 | 取值 |
|---|---|---|
| `buildable` | 一人或 2-3 人，3 个月内做得出来吗 | `yes` / `unsure` / `no` |
| `worth_it` | 有没有真实付费需求 | 同上 |
| `reachable` | 没预算没销售，触达得到用户吗 | 同上 |

三轴独立判断。「能做但卖不出去」和「好卖但做不了」是两种完全不同的结论，
后续动作也完全不同（前者找渠道，后者找合伙人或放弃），所以必须能被区分出来。

### 一句话记住整个项目的重心

**这个工具的死活只取决于一件事：AI 能不能产出非废话。**

采集、存储、TUI 都是确定性工程，写多久就是多久。只有分析质量是不确定的。
所以代码里所有看起来"多余"的机制——证据强制、快照、可重跑、过滤打标——
都是在服务这一件事。改动之前先理解这一点，否则很容易把它们当成过度设计删掉。

---

## 2. 已定下的技术决策

原因比结论重要，所以每条都带原因。要推翻某条决策，先确认它的原因是否还成立。

### 产品形态

| 决策 | 原因 |
|---|---|
| v1 主线是**深度解剖单品**，不是批量巡检 | 分析质量没验证之前跑批量，等于批量制造垃圾 |
| 产出物是**机会卡片**，不做方向假设库 | 卡片是原子单位；`thesis` / `thesis_link` 两张表已建好但没有 UI，攒够量再开 |
| **只分析评论多的产品** | 评论是用户声音的唯一硬证据源。没评论的产品，模型只能基于厂商营销话术编 |
| 门槛是**视图层的可调阈值**，不是采集时硬筛 | 阈值是拍脑袋定的，选错了不该逼你重拉数据 |
| **笔记和 AI 分析两条完全独立的线** | 分析要能随便删了重跑；笔记是你的判断，不能被覆盖 |
| v1 **不做 Web UI**，只留 HTTP API 的位置 | 聚焦。业务流程全在 `core::usecase`，server 以后只是薄包装 |

### 评分与质量

| 决策 | 原因 |
|---|---|
| 三轴**三态**（是/存疑/否），不用数字分 | 模型对 0-100 的校准很差，会扁平地全给 65-80，制造不存在的精度 |
| `pain` / `who_pays` / `gap` **必须挂原文证据** | 没有证据的判断是负资产——看起来像信息，实际会误导决策 |
| 证据引文**不翻译** | 翻译会毁掉可核验性。你回头想验证时得能拿这句话去原页面搜到 |
| 每张卡**必须写「这是个坑」** | 对冲模型的附和倾向。不强制它写反对意见，它会把一切都说成机会 |
| 竞品**允许返回空**，每个附「我知道它是因为…」 | 模型对小众赛道的竞品认知不可靠，会编出听起来很真的名字 |
| **不做**置信度标注 | 讨论过，选择不加——三条硬约束已经够重 |

### 技术栈

| 决策 | 原因 |
|---|---|
| **SQLite + FTS5** | 单文件、零运维。语义检索（sqlite-vec）留到以后 |
| **OpenRouter** + `deepseek/deepseek-v4.1-flash` | $0.15/$0.60 per M，1M 上下文，支持 structured outputs。单卡约 0.2-0.3 美分——**成本不是设计约束** |
| 必须传 `provider.require_parameters: true` | **OpenRouter 的结构化输出支持是 provider 级而非 model 级的**。不传可能被路由到不支持 `json_schema` 的 provider |
| JSON Schema 由 **schemars 从 struct 生成** | struct 和 schema 永远一致。文档注释会变成 schema 的 `description`，约束写在类型上比写在 prompt 里更不容易被忽略 |
| OpenRouter 调用用**裸 reqwest**，不用 async-openai | 要塞 `provider` 和 `usage.include` 两个非 OpenAI 标准字段，带类型的 SDK 装不下。就一个 endpoint |
| PH GraphQL **手写查询串**，不用 codegen | 只有三个 query，codegen 的维护成本大于收益 |
| sqlx 用**运行期 `query`**，不用 `query!` 宏 | 宏要求编译期能连上真实数据库（`DATABASE_URL`），对自用工具不值 |
| 归一化实体 + `Source` trait，v1 只实现 PH | 加第二个源时不用动表结构和用例层 |
| **两阶段采集** | PH 限流按复杂度算（6250 points / 15 分钟，嵌套连接相乘）。`posts(first:50){comments(first:30)}` 会迅速烧光额度。**动机是 PH 配额，不是模型成本** |
| `analysis` 表**追加不覆盖**，带输入快照 | 没有历史就没法 diff，就没法判断 prompt 改好了还是改坏了 |
| 评论过滤**只打标不删除** | 第一版规则一定不准。打标意味着改规则重跑即可，不必再花 PH 配额重抓 |

---

## 3. 文件清单

全部为新建（这是一个全新仓库，尚未 `git init`）。

### 根目录

| 文件 | 行数 | 说明 |
|---|---|---|
| `Cargo.toml` | 50 | workspace，依赖统一在 `[workspace.dependencies]` |
| `config.example.toml` | 43 | 拷成 `config.toml` 用。`config.toml` 已在 `.gitignore` |
| `README.md` | 138 | 上手、命令速查、v0.1 验收门 |
| `DESIGN.md` | — | 完整设计推导 |
| `.gitignore` | 12 | 忽略 `target/`、`config.toml`、`*.db` |
| `migrations/0001_init.sql` | 146 | 全部表 + FTS5 虚拟表 + 触发器 |
| `prompts/card.v1.md` | 87 | prompt 模板。front matter 带 `version:`，改内容就换版本号 |

### `crates/core` —— 领域内核

| 文件 | 行数 | 说明 |
|---|---|---|
| `src/model.rs` | 263 | `Item` / `Comment` / **`OpportunityCard`**。卡片 struct 同时是 schema 来源和反序列化目标 |
| `src/ports.rs` | 62 | `Source` 和 `Analyzer` 两个 trait。**定义在 core 而不是各自 crate**，避免循环依赖 |
| `src/usecase.rs` | 217 | **所有业务流程**：`ingest_url` / `analyze` / `reanalyze` / `sync` / `hydrate` |
| `src/db.rs` | 399 | SQLite 仓储。`insert_analysis` 在一个事务里同时写 analysis、拆 evidence、更新 card_fts |
| `src/filter.rs` | 250 | 评论预过滤 + 喂给模型的文本渲染（带 `[votes=N]` / `[maker]` 标记） |
| `src/render.rs` | 206 | 卡片 → markdown；过滤报告；两版卡片 diff |
| `src/config.rs` | 135 | figment 加载，`PHI_` 前缀环境变量覆盖，层级用双下划线 |
| `src/lib.rs` | 33 | 模块声明 + `Ctx` |
| `tests/pipeline.rs` | 367 | **端到端集成测试**，用假 Source / 假 Analyzer 跑通全链路 |

### `crates/sources` —— 信息源

| 文件 | 行数 | 说明 |
|---|---|---|
| `src/producthunt.rs` | 534 | PH GraphQL adapter。查询串、复杂度令牌桶、URL 解析、maker 判定 |
| `src/lib.rs` | 12 | 导出 |

### `crates/analyzer` —— AI 分析

| 文件 | 行数 | 说明 |
|---|---|---|
| `src/openrouter.rs` | 193 | OpenRouter 调用。裸 reqwest，带 `require_parameters` |
| `src/schema.rs` | 166 | schemars 生成 + 严格模式后处理（全字段 required、禁额外字段、`$defs`） |
| `src/prompt.rs` | 170 | 模板加载、版本解析、占位符渲染 |
| `src/lib.rs` | 10 | 导出 |

### `crates/cli` / `crates/server`

| 文件 | 行数 | 说明 |
|---|---|---|
| `cli/src/main.rs` | 311 | clap 子命令。**只是 `usecase` 的薄包装**，不含业务逻辑 |
| `server/src/lib.rs` | 24 | v0.4 占位。现在只有一个函数列出计划中的端点 |

### 当前验证状态

```
cargo check --workspace --all-targets   零警告
cargo clippy --workspace --all-targets  零警告
cargo test --workspace                  22 passed / 0 failed
phi --help / phi schema / phi ls        可运行，migrations 正常建表
```

**注意**：以上全部是离线验证。真实的 ProductHunt API 调用和真实的模型调用**都还没跑过**。

---

## 4. 下一步（按优先级）

### P0 —— 先跑通真实调用，再谈别的

拿两个 key（`PH_TOKEN`、`OPENROUTER_API_KEY`），跑 `phi add <某个 PH URL>`。

第一次跑大概率会在 PH 的 GraphQL 上报错（见第 5 节）。错误信息会把完整的 GraphQL
error 打出来，照着改 `crates/sources/src/producthunt.rs` 里的查询串。

同时用 `--log debug` 看一眼 `PH 配额` 那行，记下 `comments(first:50)` 的实际复杂度成本。
这个数决定后面 `sync` / `hydrate` 的批量大小能开多大。

### P0 —— 验收门：10 个产品

挑 10 个**你本来就熟**的产品跑一遍。对每张卡问：

> 这张卡有没有告诉我一件我原本不知道、并且会影响我判断的事？

**7 张以上是正确的废话，就停下改 prompt，不要往下做任何功能。**
后面所有功能都是在放大 v0.1 的输出质量；质量是负的时候，放大只会让它更难看。

归因步骤（别省）：卡片不行的时候，你分不清是 prompt 烂还是模型弱。用

```bash
phi reanalyze <analysis_id> --model <更强的模型> --diff
```

重放同一份输入快照，只变模型这一个变量。成本几毛钱，能省掉在 prompt 上白耗的好几天。

同时用 `phi filtered <item_id>` 抽查过滤误杀率，调 `config.toml` 里的规则。

### P1 —— v0.2 存量与检索

`phi sync` / `phi hydrate` 的代码已经写好了但**没跑过真实分页**。需要验证：
游标续传、复杂度令牌桶在配额耗尽时的 sleep 行为、`posts` 查询的可选参数内联是否被 PH 接受。

然后是 `phi ls` / `phi search` 的实际可用性（已实现，但只在空库上跑过）。

### P2 —— v0.3 TUI

ratatui 三栏：左列表（可筛可排）、中卡片、右笔记。
笔记独立一栏而不是卡片的一个字段——这是「两条线分离」在 UI 上的体现。

### P3 —— v0.4 HTTP API

`crates/server` 现在是空壳。把 `core::usecase` 的函数包成 axum handler 即可，
不需要重新实现任何流程。计划端点写在 `server/src/lib.rs` 的文档注释里。

### 更远

`thesis` 的 UI（表已建好）、第二个 Source、sqlite-vec 语义检索。

---

## 5. 已知的坑与待验证假设

按「踩到的概率 × 踩到时的疼痛程度」排序。

### 🔴 PH 的 GraphQL 字段名没有经过真实调用验证

`crates/sources/src/producthunt.rs` 里的三个查询串是照文档手写的，**一次都没真的发出去过**。
字段名、连接参数、`ID` vs `String` 的 scalar 类型都可能对不上。

已经做的降险：`order` 参数**故意不传**（枚举值名不确定）；`topic` / `postedAfter` /
`postedBefore` **内联成字面量**而不是声明成 GraphQL 变量（scalar 类型名不好确定）。

出错时 `gql()` 会打印完整的 GraphQL errors 数组。照着改查询串即可，其它层不受影响。

### 🔴 分析质量完全未知

默认模型 `deepseek/deepseek-v4.1-flash` **发布于 2026-09-10**，非常新。
它在「从零散用户评论里做商业判断」这类任务上没有任何可参考的经验数据。

这不是反对用它——便宜、支持结构化输出、1M 上下文，作为默认值很合理。
但它把验收门的归因问题放大了。上面 P0 里的 `--model` 重放步骤就是为这个准备的。

### 🟡 OpenRouter 的 provider 路由会漂

同一个 model slug 在不同时间可能落到不同 provider，输出风格会变。
`analysis.provider` 列记录了每次实际路由到哪，质量突然波动时**第一个查它**。

另外：结构化输出支持是 provider 级的。如果看到 `response_format` 相关的报错，
先确认 `config.toml` 里 `analyzer.require_parameters = true`。

### 🟡 `usage.include` 的响应字段是假设的

`openrouter.rs` 里假设响应带 `usage.cost` 和顶层 `provider` 字段。
代码对缺失是容忍的（全是 `Option`），所以拿不到只会让 `cost_usd` 为空，不会报错。
但如果你发现成本列一直是空的，去看一眼实际响应结构。

### 🟡 `min_length` 按字符数算，默认值是照英文调的

中文单字信息量远高于英文。60 个中文字符已经是一条有内容的抱怨，
而 60 个英文字符基本就是 "Congrats, looks awesome!"。

PH 评论以英文为主所以现在没问题，**但接中文源（36kr 之类）时必须调低**，否则会误杀一片。
更彻底的做法是按源配置阈值——`Source` 抽象已经在了，但 `CommentFilterConfig` 目前是全局的。

### 🟡 评论过滤第一版一定不准

所以被滤掉的**只打标不删除**（`comment.kept = 0` + `filter_reason`）。
跑完前 20 个产品后用 `phi filtered <id>` 抽查，改了规则重跑过滤即可，不必重抓。

### 🟢 评论翻页有个 10 页的保险丝

`fetch_discussion` 最多翻 10 页（500 条）。防止评论上千的爆款把 PH 配额吸干。
如果发现热门产品的评论被截断，调 `producthunt.rs` 里那个 `page_no >= 10`。

### 🟢 `card_fts` 是独立表，不跟 `analysis` 联动删除

`item_fts` 和 `note_fts` 有触发器维护，`card_fts` 是在 `insert_analysis` 里手动写的。
删 analysis 不会清 card_fts 的对应行。目前没有删 analysis 的路径，所以还不是问题，
但如果你加了删除功能，记得一并处理。

---

## 6. 改代码前请先读这两条

**业务流程住在 `core::usecase`。** CLI 里不要写流程，server 以后也不要。
这是「v1 不做 Web 但以后不返工」的全部依据——破坏它，加 Web 时就得重写一遍。

**`analysis` 表永远只追加。** 任何"优化"成覆盖更新的改动都会毁掉 prompt 回归能力，
而 prompt 回归是这个项目唯一的质量抓手。
