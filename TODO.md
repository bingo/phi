# TODO

## 🔴 maker 识别失效（PH 评论作者被 REDACTED）

**现象**（2026-09-15，`phi add https://www.producthunt.com/products/caddi-3` 实测）：
PH API 对第三方应用返回的评论作者是 `[REDACTED]`，`comment.is_maker` 因此恒为 0。
`post.makers` 本身能正常拿到（id / name / username 都有），但评论上的 `user` 被抹掉，
`fetch_discussion` 里「拿 makers 的 id 和评论 user.id 比对」的做法永远对不上。

**影响**：

- `comment_filter.keep_all_maker_comments` 实际不生效 —— maker 的短评论可能被 `too_short` / `noise_pattern` 误杀
- 喂给模型的文本里没有 `[maker]` 标记。卡片里的「maker 评论（厂商自述）」是模型从正文
  （"I'm Jason, one of the…"）自己推断的，这次推对了，但不可靠
- 「厂商自述 vs 用户证据」是卡片质量的关键区分，这个信号现在完全依赖模型猜测

**待查 / 可选方向**：

1. 先确认是 token 类型的限制还是 PH 全局策略：试 OAuth user token 看 `user` 是否仍被隐藏；
   以及 `Comment.userId` 字段是否同样被抹（introspection 里有这个字段，还没单独试过）
2. 查 PH schema 里有没有别的能标出 maker 的字段（比如 comment 上的 badge / isMaker 类字段）
3. 都不行的话：退化为交给模型基于正文判断，并在渲染层明确标注「maker 身份为推断」；
   或者用 makers 的 name 在正文里做启发式匹配（"I'm <first name>"、"our team"），但误报风险要评估

**可用于校验**：`Post.makerReplies`（Int）给出 maker 回复的**条数**（Caddi = 18）。认不出是哪几条，
但修好之后可以拿它核对识别出的 maker 评论数是否对得上。

**相关代码**：`crates/sources/src/producthunt.rs` 的 `to_new_comment`（`maker_ids.contains`）、
`crates/core/src/filter.rs`（`keep_all_maker_comments`、`[maker]` 渲染）

## 🟡 回复的回复（第三层）没抓

**现象**：`fetch_discussion` 只抓两层（顶层评论 + 它的 `replies`）。PH 允许继续嵌套，
Caddi 上有 1 条第三层回复被漏掉。

**为什么没直接做**：PH 单查询复杂度上限 500k，嵌套连接相乘。在评论页查询里再内联一层
`replies` 会超限（实测 `comments(first:50){ replies(first:20){ replies(first:5) } }` = 2,007,006）。

**可选方向**：
1. 先抽样几个热门产品看第三层出现频率，低于 ~2% 就不值得花配额
2. 值得的话：内联回复只要 `replies { totalCount }`，对非零的线程用 `comment(id)` 补抓。
   注意每个补抓请求固定扣 100 点配额，要和 `MAX_REPLY_REQUESTS` 共用预算

**相关代码**：`crates/sources/src/producthunt.rs` 的 `comments_query` / `replies_query` / `fetch_discussion`

## 🟡 顶层评论上限从 500 条降到了 200 条

**现象**：为了让内联回复不超单查询复杂度上限，评论页大小从 `first: 50` 降到 `COMMENT_PAGE_SIZE = 20`。
翻页保险丝 `MAX_COMMENT_PAGES = 10` 没变，所以顶层评论上限从 500 变成 200。

**背景**：保险丝的动机是配额 —— 实测每个请求固定扣 100 点（6250 / 15 分钟），和复杂度无关。
保持 10 页 = 保持单产品最多 ~1000 点的评论抓取成本。

**待定**：
1. 跑几个评论数 >200 的爆款，看被截掉的部分有没有信号（顶层评论的默认排序还没确认，
   如果是按时间，截掉的是最新的；如果是按票数，截掉的是低票的，影响小得多）
2. 如果截断有影响：提高 `MAX_COMMENT_PAGES`（成本线性增加），或者给查询加 `order: VOTES_COUNT`
   按票数取（已 introspect 确认 `CommentsOrder` = `NEWEST` | `VOTES_COUNT`），这样截掉的只会是低票评论

**相关代码**：`crates/sources/src/producthunt.rs` 的 `COMMENT_PAGE_SIZE` / `MAX_COMMENT_PAGES`

## 🟡 喂给模型的回复没有父评论上下文

**现象**：回复和顶层评论被拍平成一个列表，按票数排序后喂给模型。像 "Yes, exactly this" 或
maker 对某个质疑的回答，脱离了它回复的那条评论，模型看不出是在回应什么。

**需要的改动**：
1. `migrations/0002_*.sql`：`comment` 表加 `parent_source_comment_id TEXT`（可空）
2. `phi_core::model::NewComment` 加对应字段；`producthunt.rs` 的 `to_new_comment` 填上
   （PH `Comment.parentId` 可用，或直接用内联时的父评论 id）
3. `crates/core/src/filter.rs` 的渲染：回复缩进挂在父评论下，或加 `[reply_to=#N]` 标记
4. 过滤逻辑要想清楚：父评论被滤掉时，被保留的回复怎么渲染

**注意**：这会改变 prompt 输入格式，属于影响 `input_snapshot` 的改动，改完后新旧卡片的 diff 要考虑这个变量。

## 🟡 评论过滤规则误杀有内容的评论

**现象**（Caddi 实测，`phi filtered 1`）：

- `noise_pattern[5]`（`^\s*(nice|cool|awesome|great|amazing|wow)\b`）杀掉了
  "Wow, that's super cool. In most firms two people doing the same task do it differently so whose version
  becomes the rule? Does it even matter?" —— 以 "Wow" 开头但后面是一个真实的产品问题
- `noise_pattern[0]`（`congrat`）杀掉了
  "Congrats to the team. Good call having it stop and ask on the ambiguous ones instead of guessing." ——
  客套开头，后半句是对具体设计的认可
- `too_short` 杀掉了 "Would this work for healthcare use cases?"（41 字符）—— 短但是一个明确的需求探询

**根因**：噪声规则只要命中就杀（只要长度 < `noise_max_length = 150`），不看命中之后还剩多少内容。

**可选方向**：
1. 噪声规则改成「去掉命中的客套部分后，剩余文本仍 ≥ `min_length` 就保留」
2. 含问号的短评论豁免 `too_short`（问题本身就是需求信号）
3. 跑完前 20 个产品后再批量调，别只拿一个样本定规则

**相关代码**：`crates/core/src/filter.rs`、`config.toml` 的 `[comment_filter]`。
过滤只打标不删除，改完规则重跑过滤即可，不需要重抓。

## ✅ 已修复（2026-09-15）：`phi sync` 每次只拉到「今天」的前 200 条，报的条数有误导

**原现象**：`phi sync --since 2026-09-01` 连跑多次，每次打印「入库 200 条」但库里只多几条；拉到的全是 9-15 当天的产品。

**原因**（四个叠加）：没传 `order`，PH 默认 `RANKING` 把当天排最前；PH 忽略 `first: 50`，每页最多 20 条，
`--limit 200` 只够覆盖当天前 200 名；`sync_cursor` 表从没被读写，每次从头拉；「入库 N 条」数的是 upsert 次数。
（`posted_at` 本身没错：PH 统一在 UTC 07:01 上线。）

**修法**：
- **按 UTC 自然日切片**，每天一个 `postedAfter` / `postedBefore` 窗口
- **固定 `order: NEWEST`、`first: 20`**。实测同一天连续两次翻页 NEWEST 顺序完全一致无重复；
  VOTES 在同票条目间乱序（60 个位置只有 58 个不重复），偏移量分页会跳条
- **游标续传**：已结束的日期每翻一页存一次游标，翻完标记完成（新 migration `0002` 给 `sync_cursor` 加 `completed_at`），
  之后直接跳过；还没结束的日期（今天）新发布会推动偏移量，不存游标、不标记完成，每次从头拉
- `--limit` 换成 `--max-pages`（本次请求预算，用完即停，下次续传）；新增 `--refresh`（忽略完成标记重拉，用来刷新旧日期的票数 / 评论数）；
  `--since` / `--until` 默认今天
- 报告分开统计：新增 / 更新条数，新完成 / 跳过 / 已拉但当天未结束 / 未拉完 的天数
- 请求失败时带上下文报错，已完成的页进度保留

**验证**：9 个集成测试（切片、续传、跳过、当天不续传、失败后续传、topic 独立切片、refresh、预算用完时的计数）；
真实 API 在库副本上跑：9-14 先 `--max-pages 3` 停在偏移 60 → 重跑续传到完成，本地 724 条 = PH `totalCount` 724、无重复 →
再跑 0 请求跳过；今天 `--max-pages 2` 不留游标；跨 9-13..9-15 的范围预算用完时 9-13 续传、9-14 计为跳过。

**遗留 / 注意**：
- 全量很贵：一天 ~700 条 ≈ 37 个请求 ≈ 3700 点，9 月 1–15 日约 610 个请求，要跨 10 个配额窗口（约 2.5 小时）。
  配额低于 `complexity_floor` 时 `gql` 会自动睡到窗口重置；嫌慢就用 `--max-pages` 分批跑
- 真实库第一次运行新版本时会自动执行 migration `0002`
- 大部分产品是零票零评论的长尾，全量拉回来对 `phi ls --min-comments` 之外没什么用 —— 以后如果只想要有讨论的产品，
  只能全量拉再在视图层筛（PH 没有按评论数排序，而按票数排会跳条）
- `--refresh` 同样按全量计费

## 🟡 `phi search` 搜不到中文内容

**现象**：`item_fts` / `note_fts` / `card_fts` 三张 FTS5 表都用默认 `unicode61` 分词器，它不切中文 ——
一整句中文被当成一个 token，`phi search 律所` 命中不了卡片正文和中文笔记。

**已绕开的地方**：TUI 的 `/` 搜索改用 `db::item_ids_containing`（LIKE 子串匹配，覆盖名称 / 描述 / 卡片 / 笔记），不受影响。

**可选方向**：
1. `phi search` 直接复用 `usecase::overview` 的文本搜索（最省事，数据量下 LIKE 足够）
2. 新 migration 把 fts 表换成 `tokenize = 'trigram'`（SQLite ≥ 3.34；注意 trigram 要求查询词 ≥ 3 个字符，两字中文词仍搜不到）

**相关代码**：`crates/core/src/db.rs` 的 `search_items` / `item_ids_containing`、`migrations/0001_init.sql`

## 🟡 卡片字段退化成标签 / one_liner 照抄 tagline

**现象**（Caddi，`phi tui` 里 `[` `]` 切版本对比）：analysis #3 的 `build_cost` / `moat` / `distribution` /
`business_model` 输出成了 `high` / `weak` / `sales_led` / `b2b` 这种枚举式短词，#1 是完整句子；
#3 的 `one_liner` 直接是英文 tagline 原文，违反 schema 注释里的「不要复述 tagline」。

**变量**：#1 → #3 prompt 和模型相同，变的是输入（评论 20 → 45 条，HTML 已清洗）和 provider（Reka → Parasail）。
三次运行 provider 各不相同（Reka / Wafer / Parasail），无法区分是输入还是路由导致。

**可选方向**：
1. 用 `phi reanalyze 3` 同一快照重跑几次，看是否稳定复现（排除 provider 随机性）
2. 在 `OpportunityCard` 这几个字段的文档注释里写明「一到两句完整的话，不要用标签」—— 注释会进 JSON Schema 的 description
3. 考虑在 `analyzer.provider` 配置里固定 provider 顺序，减少路由漂移

**相关代码**：`crates/core/src/model.rs` 的 `OpportunityCard`、`prompts/card.v1.md`、`crates/analyzer/src/openrouter.rs`

## ✅ 已完成（2026-09-15）：`posts` 查询传 `order`

`PostsOrder` = `FEATURED_AT` | `VOTES` | `RANKING` | `NEWEST`。随 sync 修复一起固定为 `NEWEST`，**没有**做成 `--order` 参数：
按天切片 + 偏移量续传依赖稳定排序，只有 NEWEST 实测稳定（VOTES 会跳条，RANKING 一天内会变）。

## 🟢 PH 配额的实测规则只写在代码里

HANDOFF.md 第 2、5 节对 PH 限流的描述（「按复杂度扣配额、嵌套连接相乘」）和实测不符，但 HANDOFF 不再更新。
**以 `crates/sources/src/producthunt.rs` 的模块文档为准**：每个请求固定扣 100 点（被拒的也扣）；
另有单查询复杂度上限 500k（嵌套相乘，发请求前静态计算）。

## ✅ 已完成（2026-09-15）：TUI 待定项

**1. 在 TUI 里触发分析**
- 选中条目按 `a`，确认后（y / Enter；其他键取消）在后台跑。确认提示里写明会做什么：没抓过评论的会先抓（消耗 PH 配额）、调用模型、已有几个版本
- 流程在 core：新增 `usecase::analyze_item`，评论没抓过就先抓再分析，通过回调报告阶段（抓评论 / 调用模型）。
  `hydrate` 和它共用抽出来的 `fetch_comments_for`。`phi sync` 入库的条目现在可以直接在 TUI 里分析
- 事件循环从阻塞读改成 150ms 轮询，后台任务用 `tokio::spawn` + channel 回报进度。分析期间可以照常浏览、写笔记
- 进度显示在三处：顶栏（转圈 + 条目名 + 阶段 + 秒数）、列表行、卡片栏顶部。完成后自动刷新列表和卡片，正在看旧版本的话切回最新版本
- 同一条不能重复发起；还有分析在跑时按 `q` 要按两次才退出
- 缺 `PH_TOKEN` / `OPENROUTER_API_KEY` 时 TUI 照样能浏览，按 `a` 才提示缺什么；`phi --model <slug> tui` 会用指定模型分析

**2. Esc 紧跟字符被读成 Alt+字符**
- 普通 / 搜索 / 确认模式下，Alt+字符按「先 Esc、再这个字符」拆成两次按键处理（两个 effect 都执行）。
  笔记编辑器里不拆，Alt 组合键交给编辑器（比如 Alt+b 按词后退）

**验证**：core 新增集成测试（先抓评论再分析、缺信息源时的报错、已抓过评论时不再抓）；TUI 新增 5 个测试
（确认 / 取消、任务生命周期与重复发起、带任务退出、Esc 前缀拆分、进度渲染）；全量 46 个测试通过，clippy 零警告。
tmux 在库副本上真实跑：TryCase（#312，未抓评论）按 `a` → `y`，后台抓到 10 条评论并调用模型，68 秒完成（Parasail，$0.0059），
期间切栏、按 `q` 触发退出保护都正常，完成后卡片自动出现；快速连发 `Escape /` 现在清掉旧搜索并打开空搜索框（之前会变成「律所voice」）。

**遗留**：
- 退出时正在跑的分析会被直接中断，不会等它写完（模型费用已花）
- 没有取消单个任务的按键
- 分析不能在 TUI 里换 prompt 或用同一快照重跑（`phi reanalyze`），只能追加基于当前评论的新版本
