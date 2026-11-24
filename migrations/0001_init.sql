-- ==== 数据源与证据 ====
CREATE TABLE source (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL CHECK (kind IN ('dws_im','dws_report','manual','import')),
  display_name  TEXT NOT NULL,
  scope_json    TEXT NOT NULL,                 -- 白名单：群/会话/人 ID 列表
  enabled       INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  authorized_at INTEGER,
  revoked_at    INTEGER,
  created_at    INTEGER NOT NULL
);

CREATE TABLE source_record (
  id               TEXT PRIMARY KEY,
  source_id        TEXT NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  external_id      TEXT NOT NULL,
  kind             TEXT NOT NULL CHECK (kind IN ('im_message','weekly_report','file','manual_note')),
  occurred_at      INTEGER NOT NULL,           -- UTC ms
  ingested_at      INTEGER NOT NULL,
  conversation_id  TEXT,
  sender_id        TEXT,
  sender_name      TEXT,
  direction        TEXT NOT NULL DEFAULT 'na' CHECK (direction IN ('in','out','na')),
  summary          TEXT,
  body             TEXT,                       -- 到期可为 NULL
  body_masked      TEXT,
  body_sha256      TEXT,
  content_hash     TEXT NOT NULL,              -- NFC + sha256，去重/幂等键原料
  perm_scope       TEXT,
  raw_state        TEXT NOT NULL DEFAULT 'available'
                   CHECK (raw_state IN ('available','redacted','unavailable')),
  retention_at     INTEGER,
  judge_state      TEXT NOT NULL DEFAULT 'pending'
                   CHECK (judge_state IN ('pending','kept','skipped','judged','rejected','blocked')),
  prefilter_reason TEXT,
  UNIQUE (source_id, external_id)
);
CREATE INDEX ix_sr_occurred  ON source_record(occurred_at DESC);
CREATE INDEX ix_sr_conv      ON source_record(conversation_id, occurred_at DESC);
CREATE INDEX ix_sr_judge     ON source_record(judge_state, ingested_at);
CREATE INDEX ix_sr_retention ON source_record(retention_at) WHERE retention_at IS NOT NULL;

-- ==== 工作脉络（V0.1 只建表，不做页面）====
CREATE TABLE work_thread (
  id         TEXT PRIMARY KEY,
  title      TEXT NOT NULL,
  aliases    TEXT,                             -- JSON array
  summary    TEXT,
  status     TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open','dormant','closed')),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

-- ==== 事项 ====
CREATE TABLE work_item (
  id                     TEXT PRIMARY KEY,
  title                  TEXT NOT NULL,
  lifecycle              TEXT NOT NULL DEFAULT 'candidate'
                         CHECK (lifecycle IN ('candidate','active','closed','ignored')),
  attention              TEXT NOT NULL DEFAULT 'no_next'
                         CHECK (attention IN ('me','waiting_other','report_back','awaiting_acceptance','no_next')),
  next_action            TEXT,
  followup_type          TEXT NOT NULL DEFAULT 'none'
                         CHECK (followup_type IN ('none','event','deadline','manual_time')),
  followup_at            INTEGER,
  followup_note          TEXT,
  thread_id              TEXT REFERENCES work_thread(id) ON DELETE SET NULL,
  origin_source_record_id TEXT REFERENCES source_record(id) ON DELETE SET NULL,
  dedupe_key             TEXT NOT NULL,
  confidence             INTEGER CHECK (confidence BETWEEN 0 AND 100),
  awaiting_confirmation  INTEGER NOT NULL DEFAULT 0 CHECK (awaiting_confirmation IN (0,1)),
  closed_at              INTEGER,
  close_reason           TEXT,
  resolution_evidence    TEXT,
  last_evidence_at       INTEGER,
  created_at             INTEGER NOT NULL,
  updated_at             INTEGER NOT NULL,
  UNIQUE (dedupe_key),
  CHECK (lifecycle <> 'active'   OR (next_action IS NOT NULL AND length(trim(next_action)) > 0)),
  CHECK (attention <> 'me'       OR (next_action IS NOT NULL AND length(trim(next_action)) > 0)),
  CHECK (lifecycle <> 'closed'   OR (closed_at IS NOT NULL AND close_reason IS NOT NULL)),
  CHECK (followup_type <> 'none' OR followup_at IS NULL)
);
CREATE INDEX ix_wi_open     ON work_item(lifecycle, attention, last_evidence_at DESC);
CREATE INDEX ix_wi_thread   ON work_item(thread_id);
CREATE INDEX ix_wi_followup ON work_item(followup_at) WHERE followup_at IS NOT NULL;

-- ==== Agent 运行 ====
CREATE TABLE agent_run (
  id             TEXT PRIMARY KEY,
  kind           TEXT NOT NULL CHECK (kind IN ('classify','summarize')),
  runtime        TEXT NOT NULL,                -- cli / http / mock
  model_id       TEXT,
  prompt_version TEXT NOT NULL,
  session_ref    TEXT,
  input_refs_json TEXT NOT NULL,
  input_sha256   TEXT NOT NULL,
  output_sha256  TEXT,
  raw_output     TEXT,                         -- 原样留档，诊断用
  latency_ms     INTEGER,
  status         TEXT NOT NULL
                 CHECK (status IN ('running','ok','rejected','failed','timeout','skipped')),
  error_code     TEXT,
  rejection_rule TEXT,
  created_at     INTEGER NOT NULL,
  finished_at    INTEGER
);
-- 同输入 + 同版本 + 同模型，只允许一次成功（防重复计费与重复建项）
CREATE UNIQUE INDEX ux_agent_run_ok
  ON agent_run(kind, input_sha256, prompt_version, IFNULL(model_id,''))
  WHERE status = 'ok';
CREATE INDEX ix_ar_status ON agent_run(status, created_at DESC);

-- ==== 事件流（唯一历史真相）====
CREATE TABLE work_event (
  id               TEXT PRIMARY KEY,
  target_type      TEXT NOT NULL CHECK (target_type IN ('work_item','work_thread','source','snapshot')),
  target_id        TEXT NOT NULL,
  event_type       TEXT NOT NULL,              -- created/updated/closed/ignored/corrected/relinked/rejected/synced
  actor            TEXT NOT NULL CHECK (actor IN ('system','agent','human')),
  at               INTEGER NOT NULL,
  source_record_id TEXT REFERENCES source_record(id) ON DELETE SET NULL,
  run_id           TEXT REFERENCES agent_run(id) ON DELETE SET NULL,
  before_json      TEXT,
  after_json       TEXT,
  note             TEXT
);
CREATE INDEX ix_we_target ON work_event(target_type, target_id, at DESC);
CREATE INDEX ix_we_at     ON work_event(at DESC);

-- ==== 关联证据 ====
CREATE TABLE link_evidence (
  id               TEXT PRIMARY KEY,
  source_record_id TEXT NOT NULL REFERENCES source_record(id) ON DELETE CASCADE,
  target_type      TEXT NOT NULL CHECK (target_type IN ('work_item','work_thread')),
  target_id        TEXT NOT NULL,
  link_type        TEXT NOT NULL
                   CHECK (link_type IN ('origin','reference','mention','artifact','similar_candidate')),
  reason           TEXT,
  confidence       INTEGER,
  state            TEXT NOT NULL DEFAULT 'candidate'
                   CHECK (state IN ('candidate','confirmed','rejected')),
  decided_by       TEXT,
  created_at       INTEGER NOT NULL,
  UNIQUE (source_record_id, target_type, target_id, link_type)
);
CREATE INDEX ix_le_target ON link_evidence(target_type, target_id, state);

-- ==== 人工锁 ====
CREATE TABLE field_override (
  target_type TEXT NOT NULL CHECK (target_type IN ('work_item')),
  target_id   TEXT NOT NULL,
  field       TEXT NOT NULL,
  value_json  TEXT,
  locked      INTEGER NOT NULL DEFAULT 1 CHECK (locked IN (0,1)),
  set_at      INTEGER NOT NULL,
  set_by      TEXT NOT NULL DEFAULT 'human',
  note        TEXT,
  PRIMARY KEY (target_type, target_id, field)
);

-- ==== 同步 ====
CREATE TABLE sync_checkpoint (
  source_id             TEXT PRIMARY KEY REFERENCES source(id) ON DELETE CASCADE,
  cursor_json           TEXT,
  cursor_at             INTEGER,
  window_start          INTEGER,
  window_end            INTEGER,
  last_attempt_at       INTEGER,
  last_success_at       INTEGER,
  consecutive_failures  INTEGER NOT NULL DEFAULT 0,
  state                 TEXT NOT NULL DEFAULT 'idle'
                        CHECK (state IN ('idle','syncing','degraded','auth_required','disabled')),
  last_error_code       TEXT,
  last_error_msg        TEXT,
  updated_at            INTEGER NOT NULL
);

CREATE TABLE sync_run (
  id                 TEXT PRIMARY KEY,
  source_id          TEXT NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  started_at         INTEGER NOT NULL,
  finished_at        INTEGER,
  state              TEXT NOT NULL CHECK (state IN ('running','ok','partial','failed')),
  fetched            INTEGER NOT NULL DEFAULT 0,
  inserted           INTEGER NOT NULL DEFAULT 0,
  unchanged          INTEGER NOT NULL DEFAULT 0,
  kept_for_judgement INTEGER NOT NULL DEFAULT 0,
  error_code         TEXT,
  error_msg          TEXT
);
CREATE INDEX ix_srun ON sync_run(source_id, started_at DESC);

-- ==== 出域审计 ====
CREATE TABLE egress_log (
  id       TEXT PRIMARY KEY,
  at       INTEGER NOT NULL,
  policy   TEXT NOT NULL CHECK (policy IN ('none','redacted','full')),
  decision TEXT NOT NULL CHECK (decision IN ('blocked','allowed')),
  target   TEXT,
  refs_json TEXT NOT NULL,
  run_id   TEXT
);

-- ==== 迭代池 ====
CREATE TABLE feedback_snapshot (
  id            TEXT PRIMARY KEY,
  route         TEXT,
  target_type   TEXT,
  target_id     TEXT,
  title         TEXT NOT NULL,
  description   TEXT NOT NULL,
  expected      TEXT,
  kind          TEXT CHECK (kind IN ('wrong_owner','wrong_state','missing_item','spurious_item','wrong_link','ui','other')),
  severity      TEXT CHECK (severity IN ('low','medium','high')),
  input_json    TEXT,
  output_json   TEXT,
  log_excerpt   TEXT,
  redaction     TEXT NOT NULL DEFAULT 'masked' CHECK (redaction IN ('masked','none')),
  app_version   TEXT NOT NULL,
  schema_version INTEGER NOT NULL,
  status        TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open','resolved','wontfix')),
  fixed_in_release TEXT,
  created_at    INTEGER NOT NULL
);
CREATE INDEX ix_fb ON feedback_snapshot(status, created_at DESC);

CREATE TABLE app_release (
  version       TEXT PRIMARY KEY,
  platform      TEXT,                          -- windows-x86_64 / macos-universal
  built_at      INTEGER,
  first_seen_at INTEGER NOT NULL,
  notes         TEXT
);

-- ==== 检索 ====
CREATE VIRTUAL TABLE source_record_fts USING fts5(
  summary, body_masked,
  content='source_record', content_rowid='rowid',
  tokenize='trigram'
);

CREATE TRIGGER tr_sr_ai AFTER INSERT ON source_record BEGIN
  INSERT INTO source_record_fts(rowid, summary, body_masked)
  VALUES (new.rowid, new.summary, new.body_masked);
END;
CREATE TRIGGER tr_sr_ad AFTER DELETE ON source_record BEGIN
  INSERT INTO source_record_fts(source_record_fts, rowid, summary, body_masked)
  VALUES ('delete', old.rowid, old.summary, old.body_masked);
END;
CREATE TRIGGER tr_sr_au AFTER UPDATE ON source_record BEGIN
  INSERT INTO source_record_fts(source_record_fts, rowid, summary, body_masked)
  VALUES ('delete', old.rowid, old.summary, old.body_masked);
  INSERT INTO source_record_fts(rowid, summary, body_masked)
  VALUES (new.rowid, new.summary, new.body_masked);
END;

CREATE VIRTUAL TABLE work_item_fts USING fts5(
  title, next_action,
  content='work_item', content_rowid='rowid',
  tokenize='trigram'
);

CREATE TRIGGER tr_wi_ai AFTER INSERT ON work_item BEGIN
  INSERT INTO work_item_fts(rowid, title, next_action) VALUES (new.rowid, new.title, new.next_action);
END;
CREATE TRIGGER tr_wi_ad AFTER DELETE ON work_item BEGIN
  INSERT INTO work_item_fts(work_item_fts, rowid, title, next_action)
  VALUES ('delete', old.rowid, old.title, old.next_action);
END;
CREATE TRIGGER tr_wi_au AFTER UPDATE ON work_item BEGIN
  INSERT INTO work_item_fts(work_item_fts, rowid, title, next_action)
  VALUES ('delete', old.rowid, old.title, old.next_action);
  INSERT INTO work_item_fts(rowid, title, next_action) VALUES (new.rowid, new.title, new.next_action);
END;

-- ==== 派生视图 ====
CREATE VIEW v_item_turn AS
SELECT id,
       CASE attention
         WHEN 'me'                  THEN 'me'
         WHEN 'report_back'         THEN 'me'
         WHEN 'waiting_other'       THEN 'other'
         WHEN 'awaiting_acceptance' THEN 'other'
         ELSE 'none'
       END AS current_turn
FROM work_item;
