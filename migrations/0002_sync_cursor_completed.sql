-- sync 按天切片续传。一天的窗口结束后，那天的产品集合就固定了：翻完最后一页即标记完成，
-- 之后的 sync 直接跳过（除非 --refresh）。还没结束的那天永远不标记完成。
ALTER TABLE sync_cursor ADD COLUMN completed_at TEXT;
