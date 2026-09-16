-- phi v0.1 初始 schema —— MySQL 方言。
-- 和 migrations/sqlite/0001_init.sql 一一对应：**版本号必须对齐**，
-- 以后加迁移要同时往两个目录各放一个同号文件，否则换后端会拿到不同的 schema。
--
-- 三处刻意的偏离，理由写在各自位置：
--   1. 所有整数列统一 BIGINT —— sqlx 只把 i64 认作 BIGINT 兼容类型
--   2. 做键 / 做索引的列用 VARCHAR 而非 TEXT —— InnoDB 索引要定长前缀
--   3. 没有 FTS5 —— item_fts / note_fts 及其触发器整套不建，见文件末尾

CREATE TABLE IF NOT EXISTS item (
    id                  BIGINT       NOT NULL AUTO_INCREMENT PRIMARY KEY,
    source              VARCHAR(64)  NOT NULL,
    source_id           VARCHAR(191) NOT NULL,
    slug                VARCHAR(255),
    url                 TEXT         NOT NULL,
    name                TEXT         NOT NULL,
    tagline             TEXT,
    description         TEXT,
    website             TEXT,
    posted_at           VARCHAR(64),
    signal_count        BIGINT       NOT NULL DEFAULT 0,  -- 归一化讨论热度 = PH commentsCount
    vote_count          BIGINT       NOT NULL DEFAULT 0,
    topics              TEXT,                             -- JSON 数组
    raw                 LONGTEXT     NOT NULL,            -- 源站原始响应，保底
    fetched_at          VARCHAR(64)  NOT NULL,
    comments_fetched_at VARCHAR(64),
    UNIQUE KEY uq_item_source (source, source_id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

CREATE INDEX idx_item_signal ON item (signal_count DESC);
CREATE INDEX idx_item_posted ON item (posted_at DESC);

CREATE TABLE IF NOT EXISTS `comment` (
    id                BIGINT       NOT NULL AUTO_INCREMENT PRIMARY KEY,
    item_id           BIGINT       NOT NULL,
    source_comment_id VARCHAR(191) NOT NULL,
    author            VARCHAR(191),
    is_maker          BIGINT       NOT NULL DEFAULT 0,
    body              TEXT         NOT NULL,
    votes             BIGINT       NOT NULL DEFAULT 0,
    created_at        VARCHAR(64),
    -- 过滤结果只打标不删除：改了规则重跑过滤即可，不必再花 PH 配额重抓
    kept              BIGINT       NOT NULL DEFAULT 1,
    filter_reason     VARCHAR(191),
    UNIQUE KEY uq_comment_source (item_id, source_comment_id),
    KEY idx_comment_item (item_id, kept, votes DESC),
    CONSTRAINT fk_comment_item FOREIGN KEY (item_id) REFERENCES item (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

-- 多版本：重跑追加新行，旧行保留，这样才能 diff
CREATE TABLE IF NOT EXISTS analysis (
    id             BIGINT       NOT NULL AUTO_INCREMENT PRIMARY KEY,
    item_id        BIGINT       NOT NULL,
    created_at     VARCHAR(64)  NOT NULL,
    prompt_version VARCHAR(64)  NOT NULL,
    model          VARCHAR(191) NOT NULL,
    provider       VARCHAR(191),            -- OpenRouter 实际路由到的 provider
    input_snapshot LONGTEXT     NOT NULL,   -- 实际喂进模型的完整输入
    card           LONGTEXT     NOT NULL,   -- OpportunityCard JSON
    buildable      VARCHAR(16),
    worth_it       VARCHAR(16),
    reachable      VARCHAR(16),
    verdict        VARCHAR(16),
    usefulness     BIGINT,                  -- 预留
    tokens_in      BIGINT,
    tokens_out     BIGINT,
    cost_usd       DOUBLE,
    KEY idx_analysis_item (item_id, created_at DESC),
    CONSTRAINT fk_analysis_item FOREIGN KEY (item_id) REFERENCES item (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS evidence (
    id          BIGINT      NOT NULL AUTO_INCREMENT PRIMARY KEY,
    analysis_id BIGINT      NOT NULL,
    field       VARCHAR(32) NOT NULL,
    quote       TEXT        NOT NULL,
    source_ref  TEXT,
    KEY idx_evidence_analysis (analysis_id),
    CONSTRAINT fk_evidence_analysis FOREIGN KEY (analysis_id) REFERENCES analysis (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

-- 你的笔记：独立一条线，AI 永不触碰
CREATE TABLE IF NOT EXISTS note (
    id         BIGINT      NOT NULL AUTO_INCREMENT PRIMARY KEY,
    item_id    BIGINT      NOT NULL,
    created_at VARCHAR(64) NOT NULL,
    body       TEXT        NOT NULL,
    KEY idx_note_item (item_id, created_at DESC),
    CONSTRAINT fk_note_item FOREIGN KEY (item_id) REFERENCES item (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS sync_cursor (
    source     VARCHAR(64)  NOT NULL,
    query_key  VARCHAR(191) NOT NULL,
    -- CURSOR 是 MySQL / MariaDB 的保留字，必须反引号。DML 那边也一样（见 db.rs）——
    -- 反引号在 SQLite 里同样是合法的标识符引号，所以两个后端能共用同一条语句
    `cursor`   TEXT,
    updated_at VARCHAR(64)  NOT NULL,
    PRIMARY KEY (source, query_key)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

-- 方向假设：v1 建表不建 UI，留着以后开开关
CREATE TABLE IF NOT EXISTS thesis (
    id         BIGINT       NOT NULL AUTO_INCREMENT PRIMARY KEY,
    title      VARCHAR(255) NOT NULL,
    body       TEXT,
    status     VARCHAR(16)  NOT NULL DEFAULT 'open',
    created_at VARCHAR(64)  NOT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS thesis_link (
    thesis_id BIGINT      NOT NULL,
    item_id   BIGINT      NOT NULL,
    stance    VARCHAR(16) NOT NULL,   -- 'supports' | 'refutes'
    PRIMARY KEY (thesis_id, item_id),
    KEY idx_thesis_link_item (item_id),
    CONSTRAINT fk_thesis_link_thesis FOREIGN KEY (thesis_id) REFERENCES thesis (id) ON DELETE CASCADE,
    CONSTRAINT fk_thesis_link_item   FOREIGN KEY (item_id)   REFERENCES item (id)   ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

-- card_fts 在 SQLite 那边是 FTS5 虚表，但应用侧从来没用过它的 MATCH ——
-- `item_ids_containing` 是 LIKE 全表扫（FTS5 的 unicode61 分词器不切中文，见 db.rs 注释）。
-- 所以这里建一张同名同列的普通表就够了，DML 完全共用；`rowid` 存 analysis.id。
CREATE TABLE IF NOT EXISTS card_fts (
    rowid     BIGINT   NOT NULL PRIMARY KEY,
    card_text LONGTEXT NOT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_unicode_ci;

-- item_fts / note_fts 及其同步触发器刻意不建：
--   * note_fts 在 SQLite 侧建了但从来没被查过
--   * item_fts 只服务 `phi search`；MySQL 后端下它走 LIKE（见 db::search_items）。
--     InnoDB 的 FULLTEXT 默认分词同样不切中文，要 ngram parser，而 MariaDB 没有 ——
--     为一个自用工具赌一个可选插件不值得。
