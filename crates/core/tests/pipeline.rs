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
    let path = std::env::temp_dir().join(format!(
        "phi-test-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let pool = db::connect(&path).await.unwrap();
    db::migrate(&pool).await.unwrap();
    (Ctx::new(Config::default(), pool), path)
}

#[tokio::test]
async fn full_pipeline() {
    let (ctx, path) = ctx().await;

    let (item, analysis) =
        usecase::ingest_url(&ctx, &FakeSource, &FakeAnalyzer, "https://fake.test/posts/acme")
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
    let ev: Vec<(String, String)> =
        sqlx::query_as("SELECT field, quote FROM evidence WHERE analysis_id = ?1 ORDER BY field")
            .bind(analysis.id)
            .fetch_all(&ctx.db)
            .await
            .unwrap();
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

    let (item, first) =
        usecase::ingest_url(&ctx, &FakeSource, &FakeAnalyzer, "https://fake.test/posts/acme")
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
    let snaps: Vec<(String,)> =
        sqlx::query_as("SELECT input_snapshot FROM analysis WHERE item_id = ?1 ORDER BY id")
            .bind(item.id)
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(snaps[0].0, snaps[1].0, "重跑用的输入快照应当完全一致");

    let diff = render::diff_cards(&first, &second);
    assert!(diff.contains("三轴 A"));

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn notes_are_independent_of_analysis() {
    let (ctx, path) = ctx().await;
    let (item, analysis) =
        usecase::ingest_url(&ctx, &FakeSource, &FakeAnalyzer, "https://fake.test/posts/acme")
            .await
            .unwrap();

    usecase::add_note(&ctx, item.id, "我觉得这个方向的真问题是获客")
        .await
        .unwrap();

    // 删掉分析，笔记必须还在 —— 两条线完全分离
    sqlx::query("DELETE FROM analysis WHERE id = ?1")
        .bind(analysis.id)
        .execute(&ctx.db)
        .await
        .unwrap();

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

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM comment WHERE item_id = ?1")
        .bind(id1)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
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
