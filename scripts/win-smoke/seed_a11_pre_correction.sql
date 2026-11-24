-- WorkContext Windows 冒烟 · 隔离验收数据（合成，A11「纠错前」状态）
--
-- 这里只预置纠错发生之前的状态（fixtures/cases/A11_correction_lock 的阶段 1 终态）：
-- 一条被判读建出的「本人事项」。真正的纠错由冒烟用 WebDriver 在 release 应用里
-- 点击「不是我的事」完成（item_correct），不由本文件伪造纠错结果。
--
-- 行形态镜像真实管道（crates/app/src/judge.rs）：
--   source_record.id = "{source_id}/{external_id}"，content_hash = sha256("{source}:{ext}:{body}")
--   work_item.id = "item-" + dedupe_key 前 12 位；agent_run runtime/model 为 mock 夹具口径
--   work_event.id = "we-{at:x}-{event_type}-{target}"
--
-- 范围隔离（seed_verify.py 逐项断言）：
--   * 所有标识均带 wc-smoke-a11 合成前缀，人名/会话/系统全部虚构；
--   * 合成来源 enabled=0 且未授权：同步引擎只扫 enabled=1，未授权来源在检查点即被拒，
--     因此不可能触发任何真实同步或出域（冒烟全程不登录、不访问企业内容）；
--   * 不写 sync_checkpoint / sync_run / egress_log，不伪造任何同步或出域事实。
--
-- 幂等：seed_verify.py 先断言库内无这些标识再执行；单事务，失败整体回滚。
-- 哈希均为真实 sha256（输入为下方合成文本）：
--   body_sha256   = sha256(body)
--   content_hash  = sha256("wc-smoke-a11-src:A11-R1:" + body)
--   input_sha256  = sha256("wc-smoke|synthetic|a11-r1|" + content_hash)
--   output_sha256 = sha256(raw_output)

-- ==== 合成来源：钉钉私聊·钱经理（已停用、未授权，仅作证据标签）====
INSERT INTO source (id, kind, display_name, scope_json, enabled, authorized_at, revoked_at, created_at)
VALUES ('wc-smoke-a11-src', 'dws_im', '钉钉私聊·钱经理（合成）',
        '{"conversation_ids":["dm-qian-manager"]}', 0, NULL, 1793494800000, 1793494800000);

-- ==== 原始记录 A11-R1：私聊 direction=in → 预筛 dm_in 保留，已完成判读 ====
INSERT INTO source_record (
  id, source_id, external_id, kind, occurred_at, ingested_at,
  conversation_id, sender_id, sender_name, direction,
  summary, body, body_sha256, content_hash,
  raw_state, judge_state, prefilter_reason, mentions_json, reply_to_id, conversation_type
) VALUES (
  'wc-smoke-a11-src/A11-R1', 'wc-smoke-a11-src', 'A11-R1', 'im_message',
  1795223400000, 1795223520000,
  'dm-qian-manager', 'U_QIAN_MANAGER', '钱经理', 'in',
  '钱经理私聊让张三跟进二期图纸复核推广（消息层面归属锚点成立）',
  '张三，二期的图纸复核推广接下来你跟进吧。',
  'c003b674b4c49b53a62eb72470b2eb1d300a8b825bbd11855714042edf37e015',
  '39de8de433b8201c6d3970e0a249a166e36ad22c1c8f11056a3052dc5d4730ff',
  'available', 'judged', 'dm_in', '[]', NULL, 'dm'
);

-- ==== 入库留痕（事件流是唯一历史真相）====
INSERT INTO work_event (id, target_type, target_id, event_type, actor, at, after_json)
VALUES ('we-1a1fba90300-synced-wc-smoke-a11-src/A11-R1', 'source', 'wc-smoke-a11-src/A11-R1',
        'synced', 'system', 1795223520000, '{"prefilter":"dm_in"}');

-- ==== 判读 1（mock）：followup / belongs_to_me=yes / 置信 75，status=ok ====
INSERT INTO agent_run (
  id, kind, runtime, model_id, prompt_version, input_refs_json,
  input_sha256, output_sha256, raw_output, latency_ms, status, created_at, finished_at
) VALUES (
  'run-1a1fbb97dc0-4e4516616701-0000', 'classify', 'mock', 'fixture-classify/v1', 'classify/v1',
  '{"sourceRecordIds":[],"externalIds":["A11-R1"]}',
  '4e45166167016815f4189a7eda397d519cfe2c76dd1dfe46454a7ad32aaac8ea',
  '5f132521a80060ed8ff4fab4acbc1e49cebee7b302b8a35a3330829524785db7',
  '{"intent_class":"followup","belongs_to_me":"yes","confidence":75,"title":"二期图纸复核推广跟进","next_action":"跟进二期图纸复核推广"}',
  4, 'ok', 1795224600000, 1795224600004
);

-- ==== 事项（纠错前）：active / attention=me → 出现在今日「轮到我」====
INSERT INTO work_item (
  id, title, lifecycle, attention, next_action,
  followup_type, followup_at, followup_note, thread_id,
  origin_source_record_id, dedupe_key, confidence,
  awaiting_confirmation, closed_at, close_reason, resolution_evidence,
  last_evidence_at, created_at, updated_at
) VALUES (
  'item-wc-smoke-a11', '二期图纸复核推广跟进', 'active', 'me', '跟进二期图纸复核推广',
  'none', NULL, NULL, NULL,
  'wc-smoke-a11-src/A11-R1', 'wc-smoke-a11-r1', 75,
  0, NULL, NULL, NULL,
  1795223400000, 1795224600000, 1795224600000
);

-- ==== 证据链：原始记录 → 事项（origin / confirmed）====
INSERT INTO link_evidence (id, source_record_id, target_type, target_id, link_type, confidence, state, created_at)
VALUES ('wc-smoke-a11-src/A11-R1:item-wc-smoke-a11:origin', 'wc-smoke-a11-src/A11-R1',
        'work_item', 'item-wc-smoke-a11', 'origin', 75, 'confirmed', 1795224600000);

-- ==== 事件：created(agent) —— 纠错前该事项唯一的工作事件 ====
INSERT INTO work_event (id, target_type, target_id, event_type, actor, at, source_record_id, run_id)
VALUES ('we-1a1fbb97dc0-created-item-wc-smoke-a11', 'work_item', 'item-wc-smoke-a11',
        'created', 'agent', 1795224600000, 'wc-smoke-a11-src/A11-R1', 'run-1a1fbb97dc0-4e4516616701-0000');
