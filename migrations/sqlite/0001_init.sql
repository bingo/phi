-- phi v0.1 初始 schema
-- 设计依据见 DESIGN.md 第 4 节

CREATE TABLE IF NOT EXISTS item (
    id                  INTEGER PRIMARY KEY,
    source              TEXT    NOT NULL,
    source_id           TEXT    NOT NULL,
    slug                TEXT,
    url                 TEXT    NOT NULL,
    name                TEXT    NOT NULL,
    tagline             TEXT,
    description         TEXT,
    website             TEXT,
    posted_at           TEXT,
    signal_count        INTEGER NOT NULL DEFAULT 0,  -- 归一化讨论热度 = PH commentsCount
    vote_count          INTEGER NOT NULL DEFAULT 0,
    topics              TEXT,                        -- JSON 数组
    raw                 TEXT    NOT NULL,            -- 源站原始响应，保底
    fetched_at          TEXT    NOT NULL,
    comments_fetched_at TEXT,
    UNIQUE (source, source_id)
);

CREATE INDEX IF NOT EXISTS idx_item_signal ON item (signal_count DESC);
CREATE INDEX IF NOT EXISTS idx_item_posted ON item (posted_at DESC);

CREATE TABLE IF NOT EXISTS comment (
    id                INTEGER PRIMARY KEY,
    item_id           INTEGER NOT NULL REFERENCES item (id) ON DELETE CASCADE,
    source_comment_id TEXT    NOT NULL,
    author            TEXT,
    is_maker          INTEGER NOT NULL DEFAULT 0,
    body              TEXT    NOT NULL,
    votes             INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT,
    -- 过滤结果只打标不删除：改了规则重跑过滤即可，不必再花 PH 配额重抓
    kept              INTEGER NOT NULL DEFAULT 1,
    filter_reason     TEXT,
    UNIQUE (item_id, source_comment_id)
);

CREATE INDEX IF NOT EXISTS idx_comment_item ON comment (item_id, kept, votes DESC);

-- 多版本：重跑追加新行，旧行保留，这样才能 diff
CREATE TABLE IF NOT EXISTS analysis (
    id             INTEGER PRIMARY KEY,
    item_id        INTEGER NOT NULL REFERENCES item (id) ON DELETE CASCADE,
    created_at     TEXT    NOT NULL,
    prompt_version TEXT    NOT NULL,
    model          TEXT    NOT NULL,
    provider       TEXT,                 -- OpenRouter 实际路由到的 provider
    input_snapshot TEXT    NOT NULL,     -- 实际喂进模型的完整输入
    card           TEXT    NOT NULL,     -- OpportunityCard JSON
    buildable      TEXT,
    worth_it       TEXT,
    reachable      TEXT,
    verdict        TEXT,
    usefulness     INTEGER,              -- 预留
    tokens_in      INTEGER,
    tokens_out     INTEGER,
    cost_usd       REAL
);

CREATE INDEX IF NOT EXISTS idx_analysis_item ON analysis (item_id, created_at DESC);

CREATE TABLE IF NOT EXISTS evidence (
    id          INTEGER PRIMARY KEY,
    analysis_id INTEGER NOT NULL REFERENCES analysis (id) ON DELETE CASCADE,
    field       TEXT    NOT NULL,
    quote       TEXT    NOT NULL,
    source_ref  TEXT
);

CREATE INDEX IF NOT EXISTS idx_evidence_analysis ON evidence (analysis_id);

-- 你的笔记：独立一条线，AI 永不触碰
CREATE TABLE IF NOT EXISTS note (
    id         INTEGER PRIMARY KEY,
    item_id    INTEGER NOT NULL REFERENCES item (id) ON DELETE CASCADE,
    created_at TEXT    NOT NULL,
    body       TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_note_item ON note (item_id, created_at DESC);

CREATE TABLE IF NOT EXISTS sync_cursor (
    source     TEXT NOT NULL,
    query_key  TEXT NOT NULL,
    cursor     TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (source, query_key)
);

-- 方向假设：v1 建表不建 UI，留着以后开开关
CREATE TABLE IF NOT EXISTS thesis (
    id         INTEGER PRIMARY KEY,
    title      TEXT NOT NULL,
    body       TEXT,
    status     TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS thesis_link (
    thesis_id INTEGER NOT NULL REFERENCES thesis (id) ON DELETE CASCADE,
    item_id   INTEGER NOT NULL REFERENCES item (id) ON DELETE CASCADE,
    stance    TEXT    NOT NULL,   -- 'supports' | 'refutes'
    PRIMARY KEY (thesis_id, item_id)
);

-- 全文检索
CREATE VIRTUAL TABLE IF NOT EXISTS item_fts USING fts5 (
    name, tagline, description,
    content = 'item', content_rowid = 'id'
);

CREATE VIRTUAL TABLE IF NOT EXISTS note_fts USING fts5 (
    body, content = 'note', content_rowid = 'id'
);

-- 卡片正文拍平后写入（card_text 由应用侧拼接）
CREATE VIRTUAL TABLE IF NOT EXISTS card_fts USING fts5 (card_text);

CREATE TRIGGER IF NOT EXISTS item_ai AFTER INSERT ON item BEGIN
    INSERT INTO item_fts (rowid, name, tagline, description)
    VALUES (new.id, new.name, new.tagline, new.description);
END;

CREATE TRIGGER IF NOT EXISTS item_ad AFTER DELETE ON item BEGIN
    INSERT INTO item_fts (item_fts, rowid, name, tagline, description)
    VALUES ('delete', old.id, old.name, old.tagline, old.description);
END;

CREATE TRIGGER IF NOT EXISTS item_au AFTER UPDATE ON item BEGIN
    INSERT INTO item_fts (item_fts, rowid, name, tagline, description)
    VALUES ('delete', old.id, old.name, old.tagline, old.description);
    INSERT INTO item_fts (rowid, name, tagline, description)
    VALUES (new.id, new.name, new.tagline, new.description);
END;

CREATE TRIGGER IF NOT EXISTS note_ai AFTER INSERT ON note BEGIN
    INSERT INTO note_fts (rowid, body) VALUES (new.id, new.body);
END;

CREATE TRIGGER IF NOT EXISTS note_ad AFTER DELETE ON note BEGIN
    INSERT INTO note_fts (note_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
