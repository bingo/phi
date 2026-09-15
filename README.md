# phi

**ph**oduct**h**unt **i**nspiration tool —— 把 ProductHunt 上有真实用户讨论的产品，
拆成带证据的「机会卡片」，用来判断某个方向值不值得作为一人公司 / 小团队的产品切入点。

完整的设计依据见 [DESIGN.md](DESIGN.md)。

---

## 它回答什么

不是产品介绍，是三个判断：

1. **做得出来吗** — 一人或 2-3 人，3 个月内
2. **值得做吗** — 有没有真实付费需求
3. **卖得出去吗** — 没预算没销售，能不能触达用户

每个判断取 `是 / 存疑 / 否`，`痛点`、`谁付钱`、`空位` 三栏**必须挂原文证据**。
引不出证据的字段留空，不编。

---

## 上手

```bash
# 1. 两个 key
#    Developer Token（不过期）：https://www.producthunt.com/v2/oauth/applications
export PH_TOKEN=...
export OPENROUTER_API_KEY=...

# 2. 配置
cp config.example.toml config.toml

# 3. 编译
cargo build --release

# 4. 跑一个
./target/release/phi add https://www.producthunt.com/posts/<slug>
```

---

## v0.1 的验收门

这是这个项目唯一真正的风险点：**采集、存储、TUI 都是确定性工程，写多久就是多久；
只有「AI 能不能产出非废话」是不确定的，而且它决定这个工具的死活。**

所以在往下做任何功能之前，先跑这一步：

```bash
# 挑 10 个你本来就熟的产品
phi add <url>   # ×10
```

对每张卡问一句：**这张卡有没有告诉我一件我原本不知道、并且会影响我判断的事？**

如果 7 张以上是正确的废话，**停在这里改 prompt，不要往下做**。
后面所有功能都是在放大 v0.1 的输出质量；质量是负的时候，放大只会让它更难看。

### 归因：是 prompt 烂还是模型弱

默认模型 `deepseek/deepseek-v4.1-flash` 发布于 2026-09-10，很新，
在这类「从零散评论做商业判断」的任务上没有可参考的经验。验收门没过时先做归因：

```bash
phi reanalyze <analysis_id> --model <更强的模型> --diff
```

`reanalyze` 用存下来的输入快照重放，**不重新抓取** —— 保证只有模型这一个变量在变。
成本几毛钱，能省掉你在 prompt 上白耗的好几天。

---

## 命令

```
phi add <url>              主循环：抓取 → 评论 → 过滤 → 分析 → 打印卡片
phi sync --since 2026-09-01 [--until 2026-09-14] [--max-pages 30]   阶段一：按天批量拉轻量元数据，中断后重跑会续传
phi hydrate --min-comments 15              阶段二：给候选拉评论
phi analyze <item_id>
phi reanalyze <analysis_id> --diff
phi diff <a> <b>
phi ls --min-comments 15 --pending
phi show <item_id>
phi note <item_id> "..."
phi search "<query>"
phi filtered <item_id>     看评论过滤结果，抽查误杀率
phi schema                 打印发给模型的 JSON Schema
phi tui --min-comments 15  三栏浏览：左列表（可筛可排）、中卡片、右笔记。进去按 ? 看按键
```

---

## 两件容易忘的事

**采集是两阶段的，动机是 PH 的配额不是模型成本。**
PH 每个请求固定扣 100 点（6250 点 / 15 分钟，被拒的请求也扣），另有单查询复杂度上限，嵌套连接相乘。
一天大约 700+ 条发布、每页最多 20 条，拉一天就要 35+ 个请求。所以 `sync` 只拉轻量元数据，
`hydrate` 只对候选拉评论。模型那边一张卡约 0.2-0.3 美分，可以忽略。

**笔记和 AI 分析是两条完全独立的线。**
`analysis` 表是追加的，重跑不覆盖旧版本（没有历史就没法 diff）；
`note` 表独立存在，删掉分析笔记也还在。

---

## 布局

```
crates/
  core/       领域模型、Source/Analyzer trait、SQLite 仓储、过滤、渲染、用例层
  sources/    ProductHunt GraphQL adapter
  analyzer/   OpenRouter + schemars 生成的严格 JSON Schema
  cli/        phi 二进制
  server/     v0.4 占位
prompts/card.v1.md   prompt 模板，版本号写进每条 analysis
migrations/          SQLite schema
```

**业务流程住在 `core::usecase`，不在 CLI。** CLI 和以后的 HTTP server 都只是薄包装 ——
这是「v1 不做 Web 但以后不返工」的全部秘诀。

---

## 已知的待验证点

**PH 的 GraphQL 字段名是手写的。** 只有三个 query，codegen 的维护成本大于收益。
如果 PH 改了 schema，`gql()` 会把完整的 GraphQL 错误打出来，
照着改 `crates/sources/src/producthunt.rs` 里的查询串即可。

**复杂度成本没实测过。** `comments(first:50)` 到底吃多少 points 得跑了才知道。
第一次跑用 `--log debug` 看 `PH 配额` 那行，摸清单次成本再定批量大小。

**评论过滤第一版一定不准。** 所以被滤掉的只打标不删除。
跑完前 20 个产品后用 `phi filtered <id>` 抽查误杀率，改了规则重跑过滤即可，
不必再花 PH 配额重抓。

**`min_length` 按字符数算，默认值是照英文调的。** 中文单字信息量高得多，
以后接中文源要把它调低，否则会误杀一片。
