#!/usr/bin/env bash
# SQLite ↔ MySQL 数据搬运。默认 SQLite → MySQL，加 --reverse 反向。
#
#   scripts/dbsync.sh                # SQLite → MySQL
#   scripts/dbsync.sh --dry-run      # 先看会插入多少行
#   scripts/dbsync.sh --reverse      # MySQL → SQLite
#
# 只插入目标库缺少的行：已有的行不改，目标库多出来的行不删。可以反复跑。
#
# 两端的连接都取自 config.toml 的 [db]：SQLite 用 path，MySQL 用 url / url_env。
# 方向只由参数决定，不看 backend 那一项。
set -euo pipefail
cd "$(dirname "$0")/.."

# 必须先编译：迁移 SQL 是被 sqlx::migrate! 编译进二进制的，
# 改了 migrations/ 下的文件却跑旧二进制，会把目标库搞成半成品状态（见 MANUAL.md）
cargo build --release --bin phi
exec ./target/release/phi dbsync "$@"
