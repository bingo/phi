//! MySQL 后端的运维工具：查迁移状态，必要时清理。
//!
//! 不是测试 —— 借 `cargo test` 当入口是因为这台机器上没有 `mysql` 客户端，
//! 而为一个自用工具单独加个 bin 不值得。**默认只读**，不给 `PHI_ADMIN_ACTION` 不动任何数据。
//!
//! ```bash
//! # 看状态
//! PHI_TEST_MYSQL_URL=... cargo test -p phi-core --test mysql_admin -- --nocapture
//! # 删掉失败的迁移记录（配合手工删表，或者直接用 drop_all）
//! PHI_TEST_MYSQL_URL=... PHI_ADMIN_ACTION=clear_failed cargo test -p phi-core --test mysql_admin -- --nocapture
//! # 清空整个库（不可逆）
//! PHI_TEST_MYSQL_URL=... PHI_ADMIN_ACTION=drop_all cargo test -p phi-core --test mysql_admin -- --nocapture
//! ```
//!
//! # 它为什么存在
//!
//! MySQL / MariaDB 的 DDL 不在事务里。一条迁移中途失败，库就停在半成品状态：
//! 前面的表已经建了，`_sqlx_migrations` 里留一条 `success = false` 的记录，之后每次启动
//! 都报 `migration N is partially applied`。SQLite 那边 sqlx 把每条迁移包在事务里，
//! 失败会干净回滚，所以这个问题只在 MySQL 后端出现。
//!
//! 它还能认出**最容易误判的那种情况**：迁移 SQL 是被 `sqlx::migrate!` 编译进二进制的，
//! 改了 `migrations/` 下的文件却没重新 `cargo build`，跑的就还是旧 SQL。
//! 下面会把库里记的 checksum 和**当前二进制里嵌的** checksum 并排列出来 —— 对不上就是这个原因。

use sqlx::Row;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn inspect_or_clean() {
    let Ok(url) = std::env::var("PHI_TEST_MYSQL_URL") else {
        eprintln!("跳过：没设 PHI_TEST_MYSQL_URL");
        return;
    };
    let action = std::env::var("PHI_ADMIN_ACTION").unwrap_or_default();

    let pool = sqlx::MySqlPool::connect(&url).await.expect("连不上");
    // FOREIGN_KEY_CHECKS 是会话变量，必须和 DROP 落在同一条连接上 —— 不能让池随机分配
    let mut c = pool.acquire().await.unwrap();

    let (db, ver): (String, String) = sqlx::query_as("SELECT DATABASE(), VERSION()")
        .fetch_one(&mut *c)
        .await
        .unwrap();
    println!("库: {db}   服务器: {ver}");

    let tables: Vec<String> = sqlx::query(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = DATABASE() \
         ORDER BY table_name",
    )
    .fetch_all(&mut *c)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>(0))
    .collect();
    println!("表 {}: {}", tables.len(), tables.join(", "));

    // 当前二进制里嵌的迁移。和库里的记录比对，能认出「改了 .sql 但没重新编译」
    let embedded: Vec<(i64, String, String)> = sqlx::migrate!("../../migrations/mysql")
        .iter()
        .map(|m| (m.version, m.description.to_string(), hex(&m.checksum)))
        .collect();

    if tables.iter().any(|t| t == "_sqlx_migrations") {
        let rows = sqlx::query(
            "SELECT version, description, success, execution_time, checksum \
             FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *c)
        .await
        .unwrap();
        println!("_sqlx_migrations（库里记的）：");
        let mut stale = false;
        let mut failed = false;
        for r in &rows {
            let (v, ok) = (r.get::<i64, _>(0), r.get::<bool, _>(2));
            let sum = hex(&r.get::<Vec<u8>, _>(4));
            let mine = embedded.iter().find(|(ev, ..)| *ev == v);
            let verdict = match mine {
                None => "★ 当前二进制里没有这条迁移".into(),
                Some((_, _, es)) if *es != sum => {
                    stale = true;
                    format!("★ 和二进制里嵌的对不上（二进制: {}…）", &es[..16])
                }
                Some(_) => "checksum 一致".into(),
            };
            failed |= !ok;
            println!(
                "  v{v} {:<26} success={ok:<5} {}… {verdict}",
                r.get::<String, _>(1),
                &sum[..16]
            );
        }
        if stale {
            println!(
                "\n★ checksum 对不上意味着当时跑的是另一个版本的二进制。\n  \
                 迁移 SQL 是编译进二进制的：改了 migrations/ 下的文件必须重新 cargo build。"
            );
        }
        if failed {
            println!(
                "\n★ 有 success=false 的迁移 —— 库停在半成品状态（MySQL 的 DDL 不在事务里，\n  \
                 失败不会回滚）。自用工具最省事的修法是 PHI_ADMIN_ACTION=drop_all 清空重来；\n  \
                 想保数据就手工把那条迁移建到一半的表删掉，再 PHI_ADMIN_ACTION=clear_failed。"
            );
        }
    } else {
        println!("库里没有 _sqlx_migrations —— 还没跑过迁移");
    }
    for (v, desc, sum) in &embedded {
        println!("  二进制内嵌 v{v} {desc:<26} {}…", &sum[..16]);
    }

    match action.as_str() {
        "drop_all" => {
            sqlx::query("SET FOREIGN_KEY_CHECKS = 0")
                .execute(&mut *c)
                .await
                .unwrap();
            for t in &tables {
                sqlx::query(&format!("DROP TABLE IF EXISTS `{t}`"))
                    .execute(&mut *c)
                    .await
                    .unwrap_or_else(|e| panic!("删 {t} 失败: {e}"));
            }
            sqlx::query("SET FOREIGN_KEY_CHECKS = 1")
                .execute(&mut *c)
                .await
                .unwrap();
            let (left,): (i64,) = sqlx::query_as(
                "SELECT CAST(COUNT(*) AS SIGNED) FROM information_schema.tables \
                 WHERE table_schema = DATABASE()",
            )
            .fetch_one(&mut *c)
            .await
            .unwrap();
            assert_eq!(left, 0, "还有表没删掉");
            println!("\n已清空（{} 张表）", tables.len());
        }
        "clear_failed" => {
            let n = sqlx::query("DELETE FROM _sqlx_migrations WHERE success = FALSE")
                .execute(&mut *c)
                .await
                .unwrap()
                .rows_affected();
            println!("\n删掉 {n} 条失败的迁移记录");
        }
        "" => println!("\n（只读。要动手：PHI_ADMIN_ACTION=drop_all | clear_failed）"),
        other => panic!("不认识的 PHI_ADMIN_ACTION: {other}"),
    }
}
