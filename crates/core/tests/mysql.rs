//! MySQL 后端的集成测试。
//!
//! **默认不跑** —— 需要一台真实的 MySQL。给出 DSN 才会执行：
//!
//! ```bash
//! export PHI_TEST_MYSQL_URL='mysql://user:pass@127.0.0.1:3306/phi_test'
//! cargo test -p phi-core --test mysql -- --nocapture
//! ```
//!
//! 覆盖范围刻意就是 `db.rs` 的**全部**函数：两个后端的差异 100% 集中在那一层
//! （upsert 子句、取自增 id、COUNT 的无符号、LIKE 的转义字符、没有 FTS5），
//! 上面的 `usecase` / 渲染完全是方言无关的，由 `tests/pipeline.rs` 在 SQLite 上覆盖。
//!
//! 测试会往目标库里建表并写数据，结束时按本次运行专属的 `source` 值清干净。
//! 别指向生产库。

use phi_core::db::{self, Db};
use phi_core::model::*;

fn dsn() -> Option<String> {
    std::env::var("PHI_TEST_MYSQL_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// 本次运行专属的 source 值。并行跑、或者上次没清干净，都不会互相干扰。
fn run_tag() -> String {
    format!(
        "mysqltest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn new_item(source: &str, source_id: &str, name: &str, signal: i64) -> NewItem {
    NewItem {
        source: source.into(),
        source_id: source_id.into(),
        slug: Some(format!("slug-{source_id}")),
        url: format!("https://fake.test/posts/{source_id}"),
        name: name.into(),
        tagline: Some("给团队用的东西 —— emoji 也要活着 🎉".into()),
        description: Some("导出功能是硬伤".into()),
        website: Some("https://acme.example".into()),
        posted_at: Some("2026-09-01T00:00:00Z".into()),
        signal_count: signal,
        vote_count: 120,
        topics: vec!["productivity".into(), "saas".into()],
        raw: serde_json::json!({ "id": source_id }),
    }
}

fn card() -> OpportunityCard {
    let axis = |v: Tri, why: &str| Axis {
        value: v,
        reason: why.into(),
    };
    OpportunityCard {
        one_liner: "给小团队做数据导出的工具".into(),
        buildable: axis(Tri::Yes, "一个人能做"),
        worth_it: axis(Tri::Unsure, "付费意愿只有一条评论支撑"),
        reachable: axis(Tri::No, "用户分散，没有聚集地"),
        pain: Some("团队拿不出数据".into()),
        pain_evidence: vec![Evidence {
            quote: "导出功能是硬伤，团队数据根本拿不出来".into(),
            source_ref: "comment:c1".into(),
        }],
        who_pays: None,
        who_pays_evidence: vec![],
        build_cost: "两周".into(),
        moat: "没有".into(),
        distribution: "没有现成渠道".into(),
        business_model: "$9/mo 订阅".into(),
        gap: Some("没有批量导出".into()),
        gap_evidence: vec![Evidence {
            quote: "律所那边一次要导几百份".into(),
            source_ref: "comment:c1".into(),
        }],
        competitors: vec![],
        competitors_note: "这个细分领域我没有可靠认知".into(),
        trap: "导出这件事本身不构成一个产品".into(),
        verdict: Verdict::Watch,
        verdict_reason: "等更多付费信号".into(),
        insufficient_evidence: vec!["who_pays".into()],
    }
}

/// 把这次运行写进去的东西删掉。`card_fts` 和 `sync_cursor` 没有外键，得手动。
async fn cleanup(db: &Db, tag: &str) {
    // 按 source 取而不是 list_items —— 后者有 limit 且按热度排序，库大了会漏
    for item in db::items_by_source(db, tag).await.unwrap() {
        for a in db::analysis_history(db, item.id).await.unwrap() {
            db::delete_analysis(db, a.id).await.unwrap();
        }
        db::delete_item(db, item.id).await.unwrap();
    }
    db::delete_sync_cursors(db, tag).await.unwrap();
}

#[tokio::test]
async fn mysql_backend_round_trip() {
    let Some(url) = dsn() else {
        eprintln!("跳过：没设 PHI_TEST_MYSQL_URL");
        return;
    };

    let db = db::connect_mysql(&url, 4).await.expect("连接 MySQL 失败");
    db::migrate(&db).await.expect("跑 migrations 失败");
    assert_eq!(db.backend().as_str(), "mysql");

    let tag = run_tag();
    // 上一次跑崩了留下的残留（同一个 tag 不可能重复，这里只是防御性的）
    cleanup(&db, &tag).await;

    // ---- item：先插入，再 upsert 同一个键 ----
    let (id, inserted) = db::upsert_item_tracked(&db, &new_item(&tag, "p1", "Acme", 3))
        .await
        .expect("插入 item 失败");
    assert!(inserted, "第一次应当是新插入");
    assert!(id > 0, "自增 id 没拿到");

    let (id_again, inserted_again) =
        db::upsert_item_tracked(&db, &new_item(&tag, "p1", "Acme 改名了", 9))
            .await
            .unwrap();
    assert_eq!(
        (id_again, inserted_again),
        (id, false),
        "唯一键冲突应当走更新"
    );

    let item = db::get_item(&db, id).await.unwrap().expect("读不回来");
    assert_eq!(item.name, "Acme 改名了", "ON DUPLICATE KEY UPDATE 没生效");
    assert_eq!(item.signal_count, 9);
    assert_eq!(item.topics, vec!["productivity", "saas"], "JSON 列往返丢了");
    assert!(
        item.tagline.unwrap().contains('🎉'),
        "utf8mb4 没生效，emoji 被吃了"
    );
    assert!(item.comments_fetched_at.is_none());

    // ---- comment：两次 upsert 不该产生重复行，布尔列要能往返 ----
    let comments = vec![
        NewComment {
            source_comment_id: "c1".into(),
            author: Some("alice".into()),
            is_maker: false,
            body: "导出功能是硬伤，团队数据根本拿不出来".into(),
            votes: 12,
            created_at: None,
        },
        NewComment {
            source_comment_id: "c2".into(),
            author: Some("maker".into()),
            is_maker: true,
            body: "Pricing is $9/mo.".into(),
            votes: 0,
            created_at: None,
        },
    ];
    let decisions = vec![
        phi_core::filter::Decision {
            kept: true,
            reason: None,
        },
        phi_core::filter::Decision {
            kept: false,
            reason: Some("noise".into()),
        },
    ];
    db::upsert_comments(&db, id, &comments, &decisions)
        .await
        .unwrap();
    db::upsert_comments(&db, id, &comments, &decisions)
        .await
        .unwrap();
    assert_eq!(
        db::count_comments(&db, id).await.unwrap(),
        2,
        "重复 upsert 产生了重复行"
    );

    let all = db::load_comments(&db, id, false).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(!all[0].is_maker && all[0].votes == 12, "排序或布尔列不对");
    assert!(all[1].is_maker, "is_maker 往返丢了");
    assert!(!all[1].kept, "kept 往返丢了");
    assert_eq!(
        db::load_comments(&db, id, true).await.unwrap().len(),
        1,
        "kept_only 没过滤"
    );

    db::mark_comments_fetched(&db, id).await.unwrap();
    assert!(db::get_item(&db, id)
        .await
        .unwrap()
        .unwrap()
        .comments_fetched_at
        .is_some());

    // ---- analysis：事务里同时写 analysis + evidence + card_fts ----
    let usage = Usage {
        tokens_in: Some(1000),
        tokens_out: Some(500),
        cost_usd: Some(0.0024),
        provider: Some("FakeProvider".into()),
    };
    let snapshot = serde_json::json!({ "comments": "导出功能是硬伤" });
    let a1 = db::insert_analysis(&db, id, "vtest", "fake/model-1", &snapshot, &card(), &usage)
        .await
        .expect("写 analysis 失败");
    let a2 = db::insert_analysis(&db, id, "vtest", "fake/model-1", &snapshot, &card(), &usage)
        .await
        .unwrap();
    assert_ne!(a1, a2, "两次插入拿到了同一个自增 id");

    let got = db::get_analysis(&db, a1).await.unwrap().expect("读不回来");
    assert_eq!(got.card.verdict, Verdict::Watch, "卡片 JSON 往返丢了");
    assert_eq!(got.cost_usd, Some(0.0024), "DOUBLE 列往返丢了");
    assert_eq!(got.tokens_in, Some(1000));
    assert_eq!(got.provider.as_deref(), Some("FakeProvider"));

    let ev = db::evidence_for(&db, a1).await.unwrap();
    assert_eq!(ev.len(), 2, "证据没拆进 evidence 表");
    assert_eq!(ev[0].0, "gap");

    let history = db::analysis_history(&db, id).await.unwrap();
    assert_eq!(history.len(), 2, "重跑应当追加而不是覆盖");
    assert_eq!(
        db::latest_analysis(&db, id).await.unwrap().unwrap().id,
        history[0].id
    );
    assert!(db::get_input_snapshot(&db, a1).await.unwrap().is_some());

    // ---- note ----
    let n1 = db::insert_note(&db, id, "真问题是获客").await.unwrap();
    assert!(n1 > 0);
    let notes = db::load_notes(&db, id).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].1.contains("获客"));

    // ---- sync 游标 ----
    let key = "posts|topic=*|day=2026-09-01";
    assert!(db::get_sync_cursor(&db, &tag, key)
        .await
        .unwrap()
        .cursor
        .is_none());
    db::save_sync_cursor(&db, &tag, key, Some("20"), false)
        .await
        .unwrap();
    let cur = db::get_sync_cursor(&db, &tag, key).await.unwrap();
    assert_eq!(cur.cursor.as_deref(), Some("20"));
    assert!(
        cur.completed_at.is_none(),
        "没完成的切片不该有 completed_at"
    );
    // 同一个键再存一次：走 upsert 更新分支
    db::save_sync_cursor(&db, &tag, key, None, true)
        .await
        .unwrap();
    let cur = db::get_sync_cursor(&db, &tag, key).await.unwrap();
    assert!(
        cur.cursor.is_none() && cur.completed_at.is_some(),
        "游标更新没生效"
    );

    // ---- 浏览视图：COUNT 的无符号问题就藏在这里 ----
    let summaries = db::list_summaries(&db).await.unwrap();
    let mine = summaries
        .iter()
        .find(|s| s.item.id == id)
        .expect("列表里没有刚写的 item");
    assert_eq!(mine.analysis_count, 2, "analysis_count 不对");
    assert_eq!(mine.note_count, 1, "note_count 不对");
    assert_eq!(mine.verdict, Some(Verdict::Watch));
    assert_eq!(mine.buildable, Some(Tri::Yes));
    assert_eq!(mine.worth_it, Some(Tri::Unsure));
    assert_eq!(mine.reachable, Some(Tri::No));
    assert_eq!(mine.latest_analysis_id, Some(history[0].id));

    assert!(db::count_items(&db).await.unwrap() > 0);

    // ---- 文本命中：名称 / 卡片正文 / 笔记三路 UNION ----
    // 闭包里捕获引用而不是 Db 本身，否则第一次调用就把 db 移走了
    let d = &db;
    let hit = move |needle: &'static str| async move {
        db::item_ids_containing(d, needle)
            .await
            .unwrap()
            .contains(&id)
    };
    assert!(hit("Acme 改名").await, "名称没命中");
    assert!(hit("团队拿不出数据").await, "卡片正文没命中（card_fts）");
    assert!(hit("获客").await, "笔记没命中");
    assert!(!hit("这几个字绝对不该命中任何东西").await);
    // 通配符要被转义成字面量，不能当模式用
    assert!(!db::item_ids_containing(&db, "Acme%改名")
        .await
        .unwrap()
        .contains(&id));

    // ---- list_items 的三种 analyzed 取值 ----
    let analyzed_ids = move |analyzed| async move {
        db::list_items(d, 0, 1000, analyzed)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect::<Vec<_>>()
    };
    assert!(analyzed_ids(None).await.contains(&id));
    assert!(
        analyzed_ids(Some(true)).await.contains(&id),
        "已分析过滤不对"
    );
    assert!(
        !analyzed_ids(Some(false)).await.contains(&id),
        "未分析过滤不对"
    );
    // 阈值高于这条的 signal_count 就该被挡掉
    assert!(!db::list_items(&db, 100, 1000, None)
        .await
        .unwrap()
        .into_iter()
        .any(|i| i.id == id));

    // ---- search：MySQL 后端走 LIKE（没有 FTS5 镜像表）----
    let found = db::search_items(&db, "Acme", 20).await.unwrap();
    assert!(found.iter().any(|i| i.id == id), "search 没命中");

    // ---- 收尾：外键级联 + 手动清 card_fts / sync_cursor ----
    cleanup(&db, &tag).await;
    assert!(
        db::get_item(&db, id).await.unwrap().is_none(),
        "清理没删掉 item"
    );
    assert_eq!(
        db::count_comments(&db, id).await.unwrap(),
        0,
        "外键级联没删评论"
    );
    assert!(db::analysis_history(&db, id).await.unwrap().is_empty());
    assert!(db::load_notes(&db, id).await.unwrap().is_empty());
    assert!(db::get_sync_cursor(&db, &tag, key)
        .await
        .unwrap()
        .cursor
        .is_none());
    assert!(
        !db::item_ids_containing(&db, "团队拿不出数据")
            .await
            .unwrap()
            .contains(&id),
        "card_fts 的残留没清掉"
    );
}

/// `db::transfer` 的三条不变量：**只增、不改、不删**，而且要幂等。
///
/// 源库用一个临时 SQLite 文件，目标库是真实 MySQL。id 重映射是这里唯一真正难的东西 ——
/// 两个库的自增 id 必然不同，子表必须跟着父行在目标库里的新 id 走。
#[tokio::test]
async fn transfer_only_inserts_missing_rows() {
    let Some(url) = dsn() else {
        eprintln!("跳过：没设 PHI_TEST_MYSQL_URL");
        return;
    };
    let tag = run_tag();
    let path = std::env::temp_dir().join(format!("phi-transfer-{tag}.db"));
    let src = db::connect_sqlite(&path, 2).await.unwrap();
    db::migrate(&src).await.unwrap();
    let dst = db::connect_mysql(&url, 4).await.unwrap();
    db::migrate(&dst).await.unwrap();

    // ---- 源库：两条 item，第一条带评论 / 两个版本的分析 / 一条笔记 ----
    let a = db::upsert_item(&src, &new_item(&tag, "t1", "Alpha", 5))
        .await
        .unwrap();
    db::upsert_item(&src, &new_item(&tag, "t2", "Beta", 1))
        .await
        .unwrap();

    let comments = vec![
        NewComment {
            source_comment_id: "c1".into(),
            author: Some("alice".into()),
            is_maker: false,
            body: "导出功能是硬伤".into(),
            votes: 7,
            created_at: None,
        },
        NewComment {
            source_comment_id: "c2".into(),
            author: Some("maker".into()),
            is_maker: true,
            body: "Pricing is $9/mo.".into(),
            votes: 0,
            created_at: None,
        },
    ];
    let decisions = vec![
        phi_core::filter::Decision {
            kept: true,
            reason: None,
        },
        phi_core::filter::Decision {
            kept: false,
            reason: Some("noise".into()),
        },
    ];
    db::upsert_comments(&src, a, &comments, &decisions)
        .await
        .unwrap();

    let usage = Usage {
        tokens_in: Some(10),
        tokens_out: Some(20),
        cost_usd: Some(0.001),
        provider: Some("FakeProvider".into()),
    };
    let snap = serde_json::json!({ "comments": "导出功能是硬伤" });
    for _ in 0..2 {
        db::insert_analysis(&src, a, "vtest", "fake/model-1", &snap, &card(), &usage)
            .await
            .unwrap();
    }
    db::insert_note(&src, a, "真问题是获客").await.unwrap();
    db::save_sync_cursor(&src, &tag, "day=2026-09-01", Some("20"), true)
        .await
        .unwrap();

    // 目标库独有的一条：用来证明搬运不会删目标库的数据
    db::upsert_item(&dst, &new_item(&tag, "only-in-target", "OnlyTarget", 0))
        .await
        .unwrap();

    // ---- 第一次：全部新增 ----
    let rep = db::transfer::run(&src, &dst, false).await.unwrap();
    assert_eq!((rep.items.inserted, rep.items.skipped), (2, 0));
    assert_eq!(rep.comments.inserted, 2);
    assert_eq!(rep.analyses.inserted, 2, "两个版本都要搬");
    assert_eq!(rep.evidence.inserted, 4, "每个版本 2 条证据");
    assert_eq!(rep.card_fts.inserted, 2);
    assert_eq!(rep.notes.inserted, 1);
    assert_eq!(rep.cursors.inserted, 1);
    assert_eq!(rep.orphans, 0, "不该有挂不上父行的子表数据");

    // ---- 幂等 ----
    let again = db::transfer::run(&src, &dst, false).await.unwrap();
    assert_eq!(again.inserted_total(), 0, "重复跑不该再插入任何行");
    assert_eq!(again.items.skipped, 2);
    assert_eq!(again.evidence.skipped, 4, "已存在的分析不该重复搬证据");

    // ---- 不改：源库改了内容，目标库的已有行不动 ----
    db::upsert_item(&src, &new_item(&tag, "t1", "Alpha 改名了", 99))
        .await
        .unwrap();
    assert_eq!(
        db::transfer::run(&src, &dst, false)
            .await
            .unwrap()
            .inserted_total(),
        0
    );

    let in_target = db::items_by_source(&dst, &tag).await.unwrap();
    let t1 = in_target
        .iter()
        .find(|i| i.source_id == "t1")
        .expect("t1 没搬过去");
    assert_eq!(t1.name, "Alpha", "已有的行被改写了");
    assert_eq!(t1.signal_count, 5, "已有的行被改写了");

    // ---- 不删：目标库独有的那条还在 ----
    assert!(
        in_target.iter().any(|i| i.source_id == "only-in-target"),
        "目标库独有的数据被删了"
    );

    // ---- 外键重映射：子表挂在目标库的新 id 上 ----
    assert_ne!(
        t1.id, a,
        "目标库的自增 id 本就该和源库不同，否则这个测试没意义"
    );
    assert_eq!(db::analysis_history(&dst, t1.id).await.unwrap().len(), 2);
    assert_eq!(db::load_notes(&dst, t1.id).await.unwrap().len(), 1);
    assert_eq!(db::count_comments(&dst, t1.id).await.unwrap(), 2);
    let latest = db::latest_analysis(&dst, t1.id).await.unwrap().unwrap();
    assert_eq!(db::evidence_for(&dst, latest.id).await.unwrap().len(), 2);
    // card_fts 的 rowid 也重映射了：卡片正文能按新 id 搜到
    assert!(
        db::item_ids_containing(&dst, "团队拿不出数据")
            .await
            .unwrap()
            .contains(&t1.id),
        "card_fts 没跟着重映射"
    );
    assert_eq!(
        db::get_sync_cursor(&dst, &tag, "day=2026-09-01")
            .await
            .unwrap()
            .cursor
            .as_deref(),
        Some("20")
    );

    // ---- dry-run 一行都不写 ----
    db::upsert_item(&src, &new_item(&tag, "t3", "Gamma", 0))
        .await
        .unwrap();
    assert_eq!(
        db::transfer::run(&src, &dst, true)
            .await
            .unwrap()
            .items
            .inserted,
        1
    );
    assert_eq!(
        db::transfer::run(&src, &dst, true)
            .await
            .unwrap()
            .items
            .inserted,
        1,
        "dry-run 竟然真的写进去了"
    );
    assert_eq!(db::items_by_source(&dst, &tag).await.unwrap().len(), 3);

    cleanup(&dst, &tag).await;
    let _ = std::fs::remove_file(&path);
}
