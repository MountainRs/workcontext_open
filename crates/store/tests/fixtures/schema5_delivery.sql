INSERT INTO work_thread(id,title,status,created_at,updated_at)
VALUES ('old-thread','发布','dormant',10,20);

INSERT INTO ownership_card(id,profile,candidate_json,project_override)
VALUES ('follow','account-a','{"evidence":[{"cid":"chat","mid":"request"}],"sourceWork":"核对交接","project":"发布","state":"triage","work":"核对交接","action":"联系同事","when":"","sourceSummary":{"time":"","place":"","people":"","event":""}}',NULL);
INSERT INTO ownership_card(id,profile,candidate_json,project_override,present)
SELECT 'removed',profile,candidate_json,'',0 FROM ownership_card WHERE id='follow';

INSERT INTO work_item(id,title,lifecycle,attention,next_action,thread_id,origin_source_record_id,dedupe_key,created_at,updated_at)
VALUES ('follow','核对交接','active','waiting_other','等待回执','old-thread','source-request','d1',10,20);
INSERT INTO work_item(id,title,lifecycle,attention,closed_at,close_reason,dedupe_key,created_at,updated_at)
VALUES ('removed','完成交接','closed','no_next',30,'已核对','d2',10,30);
INSERT INTO ownership_conclusion(card_id,note,resolved,anchors_json)
VALUES ('removed','交接清单已签收',1,'[{"cid":"chat","mid":"request"}]');
INSERT INTO work_event(id,target_type,target_id,event_type,actor,at,source_record_id,note)
VALUES ('closed-event','work_item','removed','closed','human',30,'source-request','已核对');
INSERT INTO link_evidence(id,source_record_id,target_type,target_id,link_type,state,created_at)
VALUES ('old-link','source-request','work_item','removed','origin','confirmed',10);
