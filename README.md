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

## 存储后端

**默认 SQLite**，单文件零运维，什么都不用配。想换 MySQL 就改 `config.toml`：

```toml
[db]
backend = "mysql"          # "sqlite"（默认）| "mysql"
url_env = "PHI_DB_URL"     # export PHI_DB_URL=mysql://user:pass@host:3306/phi
# url = "mysql://..."      # 也可以直接写在这里，但 DSN 带密码，更推荐上面那行
max_connections = 4
```

也可以完全不碰配置文件，用环境变量覆盖：`PHI_DB__BACKEND=mysql`。

库不存在会尝试自动创建；建表时显式写了 `utf8mb4`，不吃服务器默认字符集的坑
（MySQL 5.7 默认 latin1，中文和 emoji 会被吞掉）。连接失败的报错做了脱敏，
只打印 `mysql://user:***@host/db`。

**两个后端唯一的行为差异是 `phi search`**：SQLite 走 FTS5，查询串是 FTS5 语法；
MySQL 没有 fts 镜像表，走**子串** LIKE，按讨论热度排序 —— 所以短查询会匹配到词内部，
`phi search AI` 会命中 "Email"。其余一切（包括 TUI 里的筛选和搜索，那条路本来就是 LIKE）
完全一致 —— 理由见 `migrations/mysql/0001_init.sql` 末尾：FTS5 的 `unicode61` 分词器不切中文，
InnoDB FULLTEXT 要 ngram 插件而 MariaDB 没有，为一个自用工具赌可选插件不值得。

迁移是两套 SQL、版本号一一对应。**加迁移时两个目录都要加同号文件**，
否则换后端会拿到不同的 schema。

MySQL 后端的集成测试默认不跑（需要真实服务器）：

```bash
export PHI_TEST_MYSQL_URL='mysql://user:pass@127.0.0.1:3306/phi_test'
cargo test -p phi-core --test mysql -- --nocapture
```

它会在目标库里建表、写数据、按本次运行专属的 `source` 值清干净。别指向生产库。

已实测：**MariaDB 11.4.8**（集成测试 + `sync` / `hydrate` / `analyze` / `note` / `show` /
`search` / `tui` 的真实跑通）。真正的 MySQL 8.x / 5.7 还没验证过，待验证项见 [TODO.md](TODO.md)。

### 两个库之间搬数据

```bash
scripts/dbsync.sh --dry-run    # 先看会插入多少行，一行都不写
scripts/dbsync.sh              # SQLite → MySQL
scripts/dbsync.sh --reverse    # MySQL → SQLite
```

**只增，不改，不删**：目标库已经有的行原样放过（哪怕内容不同），目标库多出来的行不动。
所以它是幂等的 —— 同一条命令跑第二次插入 0 行，可以当增量同步反复跑。

两端的连接都取自 `config.toml` 的 `[db]`（SQLite 用 `path`，MySQL 用 `url` / `url_env`），
**方向只由参数决定，不看 `backend` 那一项**。脚本会先 `cargo build`，免得踩下面那个旧二进制的坑。

不能直接 `INSERT ... SELECT`：`item` / `analysis` / `thesis` 的主键是自增的，两个库里同一条
数据的 id 必然不同，子表（`comment` / `evidence` / `note` / `card_fts`）照搬就会挂到错误的父行上。
所以每张父表先按**自然键**在目标库里定位或新建，再用这份映射改写子表外键。自然键和相关取舍
写在 [crates/core/src/db/transfer.rs](crates/core/src/db/transfer.rs) 的模块文档里 ——
其中 `analysis` 和 `note` 没有数据库级唯一约束，靠 `created_at` 的微秒精度去重。

`evidence` 和 `card_fts` 只跟着**新插入**的 analysis 走：它们没有可用的身份，
已存在的分析连带证据一起原样不动，否则重复跑会把证据翻倍。

### 迁移失败了怎么办（只会发生在 MySQL）

**改了 `migrations/` 下的文件一定要重新 `cargo build`。** 迁移 SQL 是被 `sqlx::migrate!`
**编译进二进制**的，改了文件不重编译，跑的还是旧 SQL —— 而且失败信息会指向新文件，很容易误判。

MySQL / MariaDB 的 DDL 不在事务里，所以一条迁移中途失败不会回滚：部分表已建，
`_sqlx_migrations` 里留一条 `success = false` 的记录，之后每次启动都报
`migration N is partially applied`。（SQLite 那边 sqlx 把每条迁移包在事务里，不会有这个问题。）

先查状态 —— 它会直接指出是「旧二进制」还是「真的写错了 SQL」：

```bash
PHI_TEST_MYSQL_URL=$PHI_DB_URL cargo test -p phi-core --test mysql_admin -- --nocapture
```

它把库里记的 checksum 和**当前二进制内嵌的** checksum 并排列出来，对不上就是没重新编译。
默认只读；要动手加 `PHI_ADMIN_ACTION=drop_all`（清空该库，不可逆）或 `=clear_failed`
（只删失败记录，需要自己先把建到一半的表删掉）。

---

## 布局

```
crates/
  core/       领域模型、Source/Analyzer trait、仓储（SQLite / MySQL）、过滤、渲染、用例层
  sources/    ProductHunt GraphQL adapter
  analyzer/   OpenRouter + schemars 生成的严格 JSON Schema
  cli/        phi 二进制
  server/     v0.4 占位
prompts/card.v1.md   prompt 模板，版本号写进每条 analysis
migrations/sqlite/   SQLite schema（默认）
migrations/mysql/    MySQL schema，版本号和 sqlite/ 一一对应
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
