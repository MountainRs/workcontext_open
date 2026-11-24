-- ==== 判读身份元数据（详细设计 §5/§6.3；B4 判读事务）====
-- 判读与预筛需要每条记录的确定性元数据；0001 的 source_record 未覆盖。
-- 写入方：sync 引擎（B3+）与合成导入（app::judge::import_case）。
-- 不回填旧行：conversation_type IS NULL 的记录视为缺元数据，
-- judge_pending 不臆断，保持 pending 并计入 skippedMeta。
ALTER TABLE source_record ADD COLUMN mentions_json TEXT NOT NULL DEFAULT '[]';   -- 被 @ 的 user_id JSON 数组
ALTER TABLE source_record ADD COLUMN reply_to_id TEXT;      -- 同源被引用消息的 external_id（续链唯一显式依据）
ALTER TABLE source_record ADD COLUMN conversation_type TEXT -- dm / group / other（V03 锚点①）
                   CHECK (conversation_type IS NULL OR conversation_type IN ('dm','group','other'));
CREATE INDEX ix_sr_reply ON source_record(source_id, reply_to_id) WHERE reply_to_id IS NOT NULL;
