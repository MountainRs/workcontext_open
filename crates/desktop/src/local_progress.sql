-- Incoming requests and the bound user's outgoing direct requests. Match only by
-- (conversation, quoted message); proximity in a busy group is not evidence.
WITH requests AS (
 SELECT m.cid,m.mid,m.body,m.occurred_at,c.title,(m.sender_id=:me) AS outgoing
 FROM messages m JOIN conversations c USING(cid)
 WHERE m.occurred_at BETWEEN :since AND :until AND m.sender_id IS NOT NULL
 AND (c.single_chat=1 OR (m.sender_id<>:me AND EXISTS (
   SELECT 1 FROM json_each(m.mentions_json) WHERE value=:me)))
 AND length(trim(m.body)) BETWEEN 2 AND 1600
 AND (instr(m.body,'帮忙') OR instr(m.body,'麻烦') OR instr(m.body,'请确认')
      OR instr(m.body,'请提供') OR instr(m.body,'请提交') OR instr(m.body,'请处理')
      OR instr(m.body,'请跟进') OR instr(m.body,'确认一下') OR instr(m.body,'看一下'))
), matched AS (
 SELECT q.*,r.mid AS reply_mid,r.occurred_at AS reply_at,r.body AS reply_body,
 row_number() OVER(PARTITION BY q.cid,q.mid ORDER BY r.occurred_at DESC,r.mid) AS seq
 FROM requests q LEFT JOIN messages r ON r.cid=q.cid AND r.reply_to=q.mid
 AND ((q.outgoing AND r.sender_id<>:me) OR (NOT q.outgoing AND r.sender_id=:me))
 AND r.occurred_at BETWEEN q.occurred_at AND :until
)
SELECT coalesce(nullif(title,''),'会话'),substr(body,1,240),occurred_at,
 reply_at,CASE WHEN length(reply_body)>240 THEN substr(reply_body,1,240)||'…' ELSE reply_body END,
 (SELECT count(*) FROM attachments a WHERE a.cid=m.cid AND a.mid=m.reply_mid),
 (SELECT count(*) FROM attachments a WHERE a.cid=m.cid AND a.mid=m.reply_mid AND a.extraction_status='extracted'),
 outgoing,(SELECT substr(a.text_content,1,180) FROM attachments a WHERE a.cid=m.cid
 AND a.mid=m.reply_mid AND a.extraction_status='extracted' AND length(trim(a.text_content))>0 ORDER BY a.id LIMIT 1),cid,mid,reply_mid
FROM matched m WHERE seq=1 ORDER BY occurred_at DESC,cid,mid
