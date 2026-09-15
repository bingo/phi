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
2. 如果截断有影响：提高 `MAX_COMMENT_PAGES`（成本线性增加），或者给查询加 `order` 参数
   按票数取（`CommentsOrder` 枚举值需要先 introspect 确认）

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
