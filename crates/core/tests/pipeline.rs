//! 端到端管线测试。
//!
//! 单元测试覆盖不到 SQL —— 而 SQL 正是这个项目里最容易悄悄写错的部分（迁移、
//! 冲突更新、事务里的证据拆表、FTS 触发器）。这里用假的 Source 和 Analyzer
//! 把整条链路真跑一遍：抓取 → 过滤 → 落库 → 分析 → 证据 → 渲染。

use anyhow::Result;
use async_trait::async_trait;

use phi_core::config::Config;
use phi_core::model::*;
use phi_core::ports::{AnalysisInput, Analyzer, Source};
use phi_core::{db, render, usecase, Ctx};

// ---------------------------------------------------------------- 假数据源

struct FakeSource;

fn long(s: &str) -> String {
    // min_length 按字符计。中文单字信息量高，同样字符数下比英文「实」得多 ——
    // 所以这里要刻意写长，才跨得过为英文语料调的那道阈值。
    format!(
        "{s} —— 这条评论刻意写得足够长，好让它稳稳通过 min_length 那道长度规则的检查，\
         不至于因为中文比英文紧凑而被误判成一句寒暄。"
    )
}

#[async_trait]
impl Source for FakeSource {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn matches_url(&self, url: &str) -> bool {
        url.contains("fake.test")
    }
    fn parse_url(&self, _url: &str) -> Result<ItemKey> {
        Ok(ItemKey::Slug("acme".into()))
    }
    async fn list(&self, _q: &ListQuery, _c: Option<&str>) -> Result<Page<NewItem>> {
        Ok(Page {
            items: vec![],
            next_cursor: None,
        })
    }
    async fn fetch_one(&self, _key: &ItemKey) -> Result<NewItem> {
        Ok(NewItem {
            source: "fake".into(),
            source_id: "p1".into(),
            slug: Some("acme".into()),
            url: "https://fake.test/posts/acme".into(),
            name: "Acme".into(),
            tagline: Some("Ship faster".into()),
            description: Some("An app for teams.".into()),
            website: Some("https://acme.example".into()),
            posted_at: Some("2026-09-01T00:00:00Z".into()),
            signal_count: 3,
            vote_count: 120,
            topics: vec!["productivity".into(), "saas".into()],
            raw: serde_json::json!({ "id": "p1" }),
        })
    }
    async fn fetch_discussion(&self, _key: &ItemKey) -> Result<Vec<NewComment>> {
        Ok(vec![
            NewComment {
                source_comment_id: "c1".into(),
                author: Some("alice".into()),
                is_maker: false,
                body: long("导出功能是硬伤，团队数据根本拿不出来"),
                votes: 12,
                created_at: None,
            },
            NewComment {
                source_comment_id: "c2".into(),
                author: Some("bob".into()),
                is_maker: false,
                // 这条应该被滤掉
                body: "Congrats on the launch! 🎉".into(),
                votes: 1,
                created_at: None,
            },
            NewComment {
                source_comment_id: "c3".into(),
                author: Some("maker".into()),
                is_maker: true,
                // 短，但因为是 maker 所以必须留下
                body: "Pricing is $9/mo.".into(),
                votes: 0,
                created_at: None,
            },
        ])
    }
}

// ---------------------------------------------------------------- 假分析器

struct FakeAnalyzer;

fn axis(v: Tri, reason: &str) -> Axis {
    Axis {
        value: v,
        reason: reason.into(),
    }
}

#[async_trait]
impl Analyzer for FakeAnalyzer {
    fn model(&self) -> &str {
        "fake/model-1"
    }
    fn prompt_version(&self) -> &str {
        "vtest"
    }
    async fn analyze(&self, input: &AnalysisInput) -> Result<(OpportunityCard, Usage)> {
        // 断言喂进来的输入确实经过了过滤和标注
        assert!(
            input.comments.contains("[maker]"),
            "maker 标记没传给模型: {}",
            input.comments
        );
        assert!(
            input.comments.contains("[votes=12]"),
            "票数没传给模型: {}",
            input.comments
        );
        assert!(
            !input.comments.contains("Congrats"),
            "寒暄评论没被滤掉: {}",
            input.comments
        );

        let card = OpportunityCard {
            one_liner: "给小团队做数据导出的工具".into(),
            buildable: axis(Tri::Yes, "纯前端加一个导出管线"),
            worth_it: axis(Tri::Unsure, "付费意愿只有一条评论支撑"),
            reachable: axis(Tri::No, "用户分散在各家 SaaS 里，没有聚集地"),
            pain: Some("现有产品导不出数据".into()),
            pain_evidence: vec![Evidence {
                quote: "导出功能是硬伤".into(),
                source_ref: "c1".into(),
            }],
            who_pays: None,
            who_pays_evidence: vec![],
            build_cost: "两周".into(),
            moat: "没有".into(),
            distribution: "没有现成渠道".into(),
            business_model: "$9/mo 订阅".into(),
            gap: Some("团队版缺失".into()),
            gap_evidence: vec![Evidence {
                quote: "团队数据根本拿不出来".into(),
                source_ref: "c1".into(),
            }],
            competitors: vec![],
            competitors_note: "这个细分领域我没有可靠认知".into(),
            trap: "导出这件事本身不构成一个产品".into(),
            verdict: Verdict::Watch,
            verdict_reason: "等更多付费信号".into(),
            insufficient_evidence: vec!["who_pays".into()],
        };

        Ok((
            card,
            Usage {
                tokens_in: Some(1000),
                tokens_out: Some(500),
                cost_usd: Some(0.0024),
                provider: Some("FakeProvider".into()),
            },
        ))
    }
}

// ---------------------------------------------------------------- 测试

async fn ctx() -> (Ctx, std::path::PathBuf) {
    // 光靠时间戳不够：macOS 的时钟只有微秒精度，并行跑的测试会拿到同一个文件名，
    // 然后在同一个库上重复跑 migration。加一个进程内计数器保证唯一。
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "phi-test-{}-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // 测试固定用 SQLite：MySQL 后端需要一台真实服务器，见 tests/mysql.rs
    let pool = db::connect_sqlite(&path, 4).await.unwrap();
    db::migrate(&pool).await.unwrap();
    (Ctx::new(Config::default(), pool), path)
}

#[tokio::test]
async fn full_pipeline() {
    let (ctx, path) = ctx().await;

    let (item, analysis) = usecase::ingest_url(
        &ctx,
        &FakeSource,
        &FakeAnalyzer,
        "https://fake.test/posts/acme",
    )
    .await
    .expect("ingest 失败");

    assert_eq!(item.name, "Acme");
    assert_eq!(item.topics, vec!["productivity", "saas"]);
    assert!(item.comments_fetched_at.is_some());

    // 过滤：3 条进来，寒暄那条被打标，maker 那条保留
    let all = db::load_comments(&ctx.db, item.id, false).await.unwrap();
    assert_eq!(all.len(), 3);
    let dropped: Vec<_> = all.iter().filter(|c| !c.kept).collect();
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].source_comment_id, "c2");
    assert!(dropped[0].filter_reason.is_some(), "被滤掉必须记录原因");

    // 分析落库
    assert_eq!(analysis.prompt_version, "vtest");
    assert_eq!(analysis.provider.as_deref(), Some("FakeProvider"));
    assert_eq!(analysis.card.verdict, Verdict::Watch);
    assert_eq!(analysis.card.buildable.value, Tri::Yes);
    assert!(analysis.card.who_pays.is_none());

    // 证据被拆进 evidence 表
    let ev = db::evidence_for(&ctx.db, analysis.id).await.unwrap();
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0].0, "gap");
    assert_eq!(ev[1].0, "pain");

    // 渲染出来的卡片带着证据和留空提示
    let md = render::card_markdown(&item, &analysis);
    assert!(md.contains("导出功能是硬伤"), "证据没渲染出来");
    assert!(md.contains("证据不足"), "留空字段没有提示");
    assert!(md.contains("FakeProvider"), "没记录实际 provider");

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn reanalyze_appends_and_keeps_history() {
    let (ctx, path) = ctx().await;

    let (item, first) = usecase::ingest_url(
        &ctx,
        &FakeSource,
        &FakeAnalyzer,
        "https://fake.test/posts/acme",
    )
    .await
    .unwrap();

    // 重跑：应当**追加**而不是覆盖 —— 没有历史就没法 diff
    let second = usecase::reanalyze(&ctx, &FakeAnalyzer, first.id)
        .await
        .unwrap();
    assert_ne!(first.id, second.id);

    let history = db::analysis_history(&ctx.db, item.id).await.unwrap();
    assert_eq!(history.len(), 2, "重跑应当保留旧版本");
    assert_eq!(history[0].id, second.id, "最新的排在前面");

    // 快照可复用：重跑没有重新抓取，输入必须一模一样
    let snap_of = |id| db::get_input_snapshot(&ctx.db, id);
    assert_eq!(
        snap_of(first.id).await.unwrap(),
        snap_of(second.id).await.unwrap(),
        "重跑用的输入快照应当完全一致"
    );

    let diff = render::diff_cards(&first, &second);
    assert!(diff.contains("三轴 A"));

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn notes_are_independent_of_analysis() {
    let (ctx, path) = ctx().await;
    let (item, analysis) = usecase::ingest_url(
        &ctx,
        &FakeSource,
        &FakeAnalyzer,
        "https://fake.test/posts/acme",
    )
    .await
    .unwrap();

    usecase::add_note(&ctx, item.id, "我觉得这个方向的真问题是获客")
        .await
        .unwrap();

    // 删掉分析，笔记必须还在 —— 两条线完全分离
    db::delete_analysis(&ctx.db, analysis.id).await.unwrap();

    let notes = db::load_notes(&ctx.db, item.id).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].1.contains("获客"));

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn refetch_updates_instead_of_duplicating() {
    let (ctx, path) = ctx().await;

    let id1 = usecase::fetch_and_store(&ctx, &FakeSource, &ItemKey::Slug("acme".into()))
        .await
        .unwrap();
    let id2 = usecase::fetch_and_store(&ctx, &FakeSource, &ItemKey::Slug("acme".into()))
        .await
        .unwrap();
    assert_eq!(id1, id2, "同一个源站 id 不应该产生第二条 item");

    let n = db::count_comments(&ctx.db, id1).await.unwrap();
    assert_eq!(n, 3, "重复抓取不应该产生重复评论");

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn analyze_refuses_when_no_comments_survive() {
    // 这个工具的整套框架建立在真实用户声音上。没有评论就分析，
    // 模型只能基于营销话术编 —— 与其产出废话，不如直接失败。
    struct Silent;
    #[async_trait]
    impl Source for Silent {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn matches_url(&self, _u: &str) -> bool {
            true
        }
        fn parse_url(&self, _u: &str) -> Result<ItemKey> {
            Ok(ItemKey::Slug("acme".into()))
        }
        async fn list(&self, _q: &ListQuery, _c: Option<&str>) -> Result<Page<NewItem>> {
            Ok(Page {
                items: vec![],
                next_cursor: None,
            })
        }
        async fn fetch_one(&self, k: &ItemKey) -> Result<NewItem> {
            FakeSource.fetch_one(k).await
        }
        async fn fetch_discussion(&self, _k: &ItemKey) -> Result<Vec<NewComment>> {
            Ok(vec![NewComment {
                source_comment_id: "c1".into(),
                author: None,
                is_maker: false,
                body: "Congrats! 🎉".into(),
                votes: 0,
                created_at: None,
            }])
        }
    }

    let (ctx, path) = ctx().await;
    let id = usecase::fetch_and_store(&ctx, &Silent, &ItemKey::Slug("acme".into()))
        .await
        .unwrap();

    let err = usecase::analyze(&ctx, &FakeAnalyzer, id).await.unwrap_err();
    assert!(
        err.to_string().contains("没有通过过滤的评论"),
        "错误信息应当解释为什么拒绝: {err}"
    );

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn overview_filters_sorts_and_searches() {
    let (ctx, path) = ctx().await;

    // A：走完整管线，有分析（watch / 能做 yes / 值得 unsure / 触达 no），3 条评论
    let (a, _) = usecase::ingest_url(
        &ctx,
        &FakeSource,
        &FakeAnalyzer,
        "https://fake.test/posts/acme",
    )
    .await
    .unwrap();
    // B：只入库没分析，评论更多
    let b_id = db::upsert_item(
        &ctx.db,
        &NewItem {
            source: "fake".into(),
            source_id: "p2".into(),
            slug: Some("zeta".into()),
            url: "https://fake.test/posts/zeta".into(),
            name: "Zeta".into(),
            tagline: Some("100% offline notes".into()),
            description: None,
            website: None,
            posted_at: Some("2026-09-10T00:00:00Z".into()),
            signal_count: 50,
            vote_count: 10,
            topics: vec![],
            raw: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    usecase::add_note(&ctx, b_id, "真问题是获客").await.unwrap();

    let names = |o: &Overview| {
        o.rows
            .iter()
            .map(|r| r.item.name.clone())
            .collect::<Vec<_>>()
    };
    let run = |q: OverviewQuery| {
        let ctx = &ctx;
        async move { usecase::overview(ctx, &q).await.unwrap() }
    };

    // 默认：全部，按评论数降序
    let all = run(OverviewQuery::default()).await;
    assert_eq!(all.total, 2);
    assert_eq!(names(&all), ["Zeta", "Acme"]);
    let acme = &all.rows[1];
    assert_eq!(acme.verdict, Some(Verdict::Watch));
    assert_eq!(acme.buildable, Some(Tri::Yes));
    assert_eq!(acme.analysis_count, 1);
    assert_eq!(all.rows[0].note_count, 1);
    assert!(all.rows[0].latest_analysis_id.is_none());

    // 分析状态
    let q = |f: fn(&mut OverviewQuery)| {
        let mut q = OverviewQuery::default();
        f(&mut q);
        q
    };
    assert_eq!(
        names(&run(q(|q| q.state = AnalysisState::Pending)).await),
        ["Zeta"]
    );
    assert_eq!(
        names(&run(q(|q| q.state = AnalysisState::Analyzed)).await),
        ["Acme"]
    );

    // 结论和三轴：「能做但卖不出去」这种组合必须能筛出来
    assert_eq!(
        names(&run(q(|q| q.verdict = Some(Verdict::Watch))).await),
        ["Acme"]
    );
    assert!(run(q(|q| q.verdict = Some(Verdict::Follow)))
        .await
        .rows
        .is_empty());
    assert_eq!(
        names(
            &run(q(|q| {
                q.buildable = Some(Tri::Yes);
                q.reachable = Some(Tri::No);
            }))
            .await
        ),
        ["Acme"]
    );
    assert!(run(q(|q| q.worth_it = Some(Tri::Yes)))
        .await
        .rows
        .is_empty());

    // 阈值
    assert_eq!(names(&run(q(|q| q.min_signal = 10)).await), ["Zeta"]);

    // 搜索：中文子串要能命中卡片正文和笔记（FTS5 默认分词器做不到这点）
    let search = |t: &'static str| q_text(t);
    assert_eq!(names(&run(search("数据导出")).await), ["Acme"]);
    assert_eq!(names(&run(search("获客")).await), ["Zeta"]);
    assert_eq!(
        names(&run(search("acme")).await),
        ["Acme"],
        "ASCII 应当大小写不敏感"
    );
    // LIKE 通配符要被转义：「100%」只该命中字面量
    assert_eq!(names(&run(search("100%")).await), ["Zeta"]);
    assert!(run(search("0%o")).await.rows.is_empty(), "% 没被转义");

    // 排序
    assert_eq!(
        names(&run(q(|q| q.sort = SortKey::Name)).await),
        ["Acme", "Zeta"]
    );
    assert_eq!(
        names(&run(q(|q| q.sort = SortKey::Votes)).await),
        ["Acme", "Zeta"]
    );
    assert_eq!(
        names(&run(q(|q| q.sort = SortKey::Newest)).await),
        ["Zeta", "Acme"]
    );

    // 详情：分析和笔记并排，但互不相干
    let d = usecase::item_detail(&ctx, a.id).await.unwrap();
    assert_eq!(d.analyses.len(), 1);
    assert!(d.notes.is_empty());
    assert_eq!(
        usecase::item_detail(&ctx, b_id).await.unwrap().notes.len(),
        1
    );

    let _ = std::fs::remove_file(path);
}

fn q_text(t: &str) -> OverviewQuery {
    OverviewQuery {
        text: Some(t.into()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- sync

/// 按天分页的假数据源：每天若干条，每页 2 条，游标 = 偏移量（和 PH 一样）。
struct PagedSource {
    /// (UTC 日期, 当天条数)
    days: Vec<(chrono::NaiveDate, usize)>,
    calls: std::sync::Mutex<Vec<(chrono::NaiveDate, usize)>>,
    /// 第 N 次调用时报错（模拟网络中断），None 表示不报错
    fail_on_call: Option<usize>,
}

impl PagedSource {
    fn new(days: Vec<(chrono::NaiveDate, usize)>) -> Self {
        Self {
            days,
            calls: Default::default(),
            fail_on_call: None,
        }
    }
    fn calls(&self) -> Vec<(chrono::NaiveDate, usize)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Source for PagedSource {
    fn id(&self) -> &'static str {
        "paged"
    }
    fn matches_url(&self, _u: &str) -> bool {
        false
    }
    fn parse_url(&self, _u: &str) -> Result<ItemKey> {
        unreachable!()
    }
    async fn list(&self, q: &ListQuery, cursor: Option<&str>) -> Result<Page<NewItem>> {
        let day = q.posted_after.expect("sync 必须按天切片").date_naive();
        assert_eq!(
            q.posted_before.unwrap() - q.posted_after.unwrap(),
            chrono::Duration::days(1),
            "切片应当正好一天"
        );
        let offset: usize = cursor.map(|c| c.parse().unwrap()).unwrap_or(0);
        let n_call = {
            let mut calls = self.calls.lock().unwrap();
            calls.push((day, offset));
            calls.len()
        };
        if self.fail_on_call == Some(n_call) {
            anyhow::bail!("模拟网络中断");
        }
        let total = self
            .days
            .iter()
            .find(|(d, _)| *d == day)
            .map_or(0, |(_, n)| *n);
        let end = (offset + 2).min(total);
        let items = (offset..end)
            .map(|i| NewItem {
                source: "paged".into(),
                source_id: format!("{day}-{i}"),
                slug: None,
                url: format!("https://paged.test/{day}/{i}"),
                name: format!("{day} #{i}"),
                tagline: None,
                description: None,
                website: None,
                posted_at: Some(format!("{day}T07:01:00Z")),
                signal_count: i as i64,
                vote_count: 0,
                topics: vec![],
                raw: serde_json::json!({}),
            })
            .collect();
        Ok(Page {
            items,
            next_cursor: (end < total).then(|| end.to_string()),
        })
    }
    async fn fetch_one(&self, _k: &ItemKey) -> Result<NewItem> {
        unreachable!()
    }
    async fn fetch_discussion(&self, _k: &ItemKey) -> Result<Vec<NewComment>> {
        unreachable!()
    }
}

fn day(s: &str) -> chrono::NaiveDate {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn sync_req(since: &str, until: &str) -> SyncRequest {
    SyncRequest {
        topic: None,
        since: day(since),
        until: day(until),
        max_pages: None,
        refresh: false,
    }
}

async fn item_count(ctx: &Ctx) -> i64 {
    db::count_items(&ctx.db).await.unwrap()
}

#[tokio::test]
async fn sync_slices_by_day_and_resumes_after_budget_runs_out() {
    let (ctx, path) = ctx().await;
    // 两个已经结束的日子：5 条 = 3 页，3 条 = 2 页
    let src = PagedSource::new(vec![(day("2026-09-01"), 5), (day("2026-09-02"), 3)]);

    // 第一次只给 2 页预算：9-01 翻到一半就停
    let r = usecase::sync(
        &ctx,
        &src,
        &SyncRequest {
            max_pages: Some(2),
            ..sync_req("2026-09-01", "2026-09-02")
        },
    )
    .await
    .unwrap();
    assert!(r.budget_exhausted);
    assert_eq!(
        (r.pages, r.inserted, r.updated, r.days_completed),
        (2, 4, 0, 0)
    );
    assert_eq!(
        (r.days, r.days_pending()),
        (2, 2),
        "没轮到的日子也要算进范围"
    );

    // 第二次不限预算：9-01 从偏移 4 续传，然后拉完 9-02
    let r = usecase::sync(&ctx, &src, &sync_req("2026-09-01", "2026-09-02"))
        .await
        .unwrap();
    assert!(!r.budget_exhausted);
    assert_eq!(r.days_completed, 2);
    assert_eq!(
        (r.pages, r.inserted, r.updated),
        (3, 4, 0),
        "续传不应重拉已拉过的页"
    );
    assert_eq!(
        src.calls(),
        vec![
            (day("2026-09-01"), 0),
            (day("2026-09-01"), 2),
            (day("2026-09-01"), 4), // ← 续传点
            (day("2026-09-02"), 0),
            (day("2026-09-02"), 2),
        ]
    );
    assert_eq!(item_count(&ctx).await, 8);

    // 预算在 9-01 用完时，排在后面、之前已完成的 9-02 应该算「跳过」而不是「未拉完」
    let r = usecase::sync(
        &ctx,
        &src,
        &SyncRequest {
            max_pages: Some(0),
            refresh: false,
            ..sync_req("2026-08-31", "2026-09-02")
        },
    )
    .await
    .unwrap();
    assert!(r.budget_exhausted);
    assert_eq!(
        (r.days, r.days_skipped, r.days_pending(), r.pages),
        (3, 2, 1, 0)
    );

    // 第三次：两天都已完成，一个请求都不发
    let r = usecase::sync(&ctx, &src, &sync_req("2026-09-01", "2026-09-02"))
        .await
        .unwrap();
    assert_eq!((r.pages, r.days_skipped, r.days), (0, 2, 2));

    // --refresh：从头重拉，全部算「更新」而不是「新增」
    let r = usecase::sync(
        &ctx,
        &src,
        &SyncRequest {
            refresh: true,
            ..sync_req("2026-09-01", "2026-09-02")
        },
    )
    .await
    .unwrap();
    assert_eq!(
        (r.pages, r.inserted, r.updated, r.days_completed),
        (5, 0, 8, 2)
    );
    assert_eq!(item_count(&ctx).await, 8);

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn sync_never_marks_an_unfinished_day_complete() {
    let (ctx, path) = ctx().await;
    let today = chrono::Utc::now().date_naive();
    let src = PagedSource::new(vec![(today, 3)]);
    let req = SyncRequest {
        topic: None,
        since: today,
        until: today,
        max_pages: None,
        refresh: false,
    };

    let r = usecase::sync(&ctx, &src, &req).await.unwrap();
    assert_eq!((r.days_open, r.days_completed, r.inserted), (1, 0, 3));

    // 再跑一次：今天还没结束，必须从头再拉（新发布的产品会把偏移量往后推，续传不可靠）
    let r = usecase::sync(&ctx, &src, &req).await.unwrap();
    assert_eq!(
        (r.days_open, r.days_skipped, r.pages, r.updated),
        (1, 0, 2, 3)
    );
    assert_eq!(src.calls().iter().filter(|(_, off)| *off == 0).count(), 2);

    // 中途被预算打断也不留游标
    let src2 = PagedSource::new(vec![(today, 3)]);
    usecase::sync(
        &ctx,
        &src2,
        &SyncRequest {
            max_pages: Some(1),
            ..req.clone()
        },
    )
    .await
    .unwrap();
    usecase::sync(&ctx, &src2, &req).await.unwrap();
    assert_eq!(src2.calls()[1], (today, 0), "当天不应续传");

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn sync_keeps_progress_when_a_request_fails() {
    let (ctx, path) = ctx().await;
    let mut src = PagedSource::new(vec![(day("2026-09-01"), 6)]);
    src.fail_on_call = Some(2);

    let err = usecase::sync(&ctx, &src, &sync_req("2026-09-01", "2026-09-01"))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("重跑会续传"), "{err:#}");
    assert_eq!(item_count(&ctx).await, 2, "失败前那一页已经入库");

    // 重跑：从偏移 2 接着拉
    src.fail_on_call = None;
    let r = usecase::sync(&ctx, &src, &sync_req("2026-09-01", "2026-09-01"))
        .await
        .unwrap();
    assert_eq!((r.inserted, r.days_completed), (4, 1));
    assert_eq!(src.calls().last().unwrap(), &(day("2026-09-01"), 4));
    assert_eq!(
        src.calls()[2],
        (day("2026-09-01"), 2),
        "应当从失败的那页重试"
    );

    // topic 不同是另一个切片：按 topic 拉完不代表全量拉完
    let r = usecase::sync(
        &ctx,
        &src,
        &SyncRequest {
            topic: Some("ai".into()),
            ..sync_req("2026-09-01", "2026-09-01")
        },
    )
    .await
    .unwrap();
    assert_eq!((r.days_skipped, r.days_completed), (0, 1));

    // 日期写反要报错
    assert!(
        usecase::sync(&ctx, &src, &sync_req("2026-09-02", "2026-09-01"))
            .await
            .is_err()
    );

    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------- analyze_item

#[tokio::test]
async fn analyze_item_fetches_comments_first_when_missing() {
    let (ctx, path) = ctx().await;
    // 模拟 sync 入库：只有元数据，没抓评论
    let item_id = db::upsert_item(
        &ctx.db,
        &FakeSource
            .fetch_one(&ItemKey::Slug("acme".into()))
            .await
            .unwrap(),
    )
    .await
    .unwrap();

    let stages = std::sync::Mutex::new(Vec::new());
    let record = |s: usecase::AnalyzeStage| stages.lock().unwrap().push(s);

    // 没有信息源又没抓过评论：说清楚缺什么，而不是报「没有评论」
    let err = usecase::analyze_item(&ctx, None, &FakeAnalyzer, item_id, &record)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("还没抓过评论"), "{err:#}");
    assert!(stages.lock().unwrap().is_empty());

    // 有信息源：先抓评论，再调模型
    let a1 = usecase::analyze_item(&ctx, Some(&FakeSource), &FakeAnalyzer, item_id, &record)
        .await
        .unwrap();
    assert_eq!(
        *stages.lock().unwrap(),
        [
            usecase::AnalyzeStage::FetchingComments,
            usecase::AnalyzeStage::CallingModel
        ]
    );
    assert_eq!(
        db::load_comments(&ctx.db, item_id, false)
            .await
            .unwrap()
            .len(),
        3
    );

    // 评论已经抓过：不再抓，也不需要信息源；追加新版本
    stages.lock().unwrap().clear();
    let a2 = usecase::analyze_item(&ctx, None, &FakeAnalyzer, item_id, &record)
        .await
        .unwrap();
    assert_eq!(
        *stages.lock().unwrap(),
        [usecase::AnalyzeStage::CallingModel]
    );
    assert_ne!(a1.id, a2.id);
    assert_eq!(
        db::analysis_history(&ctx.db, item_id).await.unwrap().len(),
        2
    );

    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------- 迁移

/// 两套迁移（SQLite / MySQL）必须版本号一一对应。
///
/// 这是整个双后端方案里唯一没法靠类型系统守住的地方：只给一边加了迁移，编译能过、
/// SQLite 的测试全绿，直到某天换成 MySQL 才发现表结构不一样。所以在这里断言文件名
/// 集合完全相同 —— sqlx 用「版本号_描述」解析迁移，文件名一致就意味着两边的版本序列一致。
#[test]
fn migrations_stay_in_sync_across_backends() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    let names = |dir: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root.join(dir))
            .unwrap_or_else(|e| panic!("读不到 migrations/{dir}: {e}"))
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        v.sort();
        v
    };
    let (sqlite, mysql) = (names("sqlite"), names("mysql"));
    assert!(!sqlite.is_empty(), "migrations/sqlite 是空的");
    assert_eq!(
        sqlite, mysql,
        "两个后端的迁移对不上。加迁移时 migrations/sqlite/ 和 migrations/mysql/ \
         必须放同号同名的文件，否则换后端会拿到不同的 schema"
    );
}
