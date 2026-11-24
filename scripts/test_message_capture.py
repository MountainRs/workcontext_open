"""Synthetic DWS projections and checkpoints. Never read an account or archive."""
import json
import sqlite3
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import archive_media
import archive_sync as sync


class CaptureTest(unittest.TestCase):
    def setUp(self):
        self.conn = sqlite3.connect(':memory:')
        self.addCleanup(self.conn.close)
        self.conn.execute('PRAGMA foreign_keys=ON')
        self.conn.executescript(sync.SCHEMA)
        self.media = Mock()
        self.media.init.side_effect = archive_media.init
        self.window = {'start_ms': 1000, 'end_ms': 2000, 'cursor': '0'}

    def save(self, entry, **kwargs):
        with self.conn:
            sync.save_messages(self.conn, [{'openConversationId': 'c', 'messages': [
                {'messageId': 'outer', 'createTime': 1500, **entry}]}], self.window,
                self.media, 'self-alias', 'me', **kwargs)

    def row(self, sql):
        return self.conn.execute(sql).fetchone()

    def test_structured_and_serialized_content_with_separate_projection(self):
        obj = {'textContent': {'text': '请核对原始数据'}, 'contentType': 1}
        encoded = json.dumps(obj, ensure_ascii=False, indent=2)
        for value in (obj, encoded):
            with self.subTest(value=type(value)):
                self.save({'text': {'unsupported': 'not speech'}, 'content': value,
                           'messageType': 'richText', 'threadId': 'thread-1'})
                body, kind, raw, thread = self.row('''SELECT body,message_type,raw_json,thread_id
                    FROM messages JOIN message_capture USING(cid,mid)''')
                self.assertEqual((body, kind, thread), ('请核对原始数据', 'richText', 'thread-1'))
                self.assertEqual(json.loads(raw), {'text': {'unsupported': 'not speech'},
                                                   'content': value, 'createTime': 1500})
        self.save({'sendType': 'unknown', 'text': encoded})
        self.assertEqual(self.row('SELECT message_type FROM message_capture'), ('1',))
        self.save({'content': [{'text': '甲'}, {'textContent': {'text': '乙'}}]})
        self.assertEqual(self.row('SELECT body FROM messages'), ('甲\n乙',))
        self.save({'content': {'resourceId': 'some identifier'}})
        self.assertIsNone(self.row('SELECT body FROM messages')[0])

    def test_reference_identity_is_not_parent_speech_or_another_conversations_reply(self):
        self.save({'text': '这是我写的', 'quotedMessage': {'openConversationId': 'other',
            'openMessageId': 'original', 'content': '仅引用', 'createTime': 800,
            'senderOpenDingTalkId': 'self-alias', 'sendType': 'text'},
            'forwarded': [{'messageId': 'f1', 'sender': '另一个人', 'createTime': 1200,
                           'text': {'textContent': {'text': '仅转发'}},
                           'quotedMessage': {'messageId': 'nested', 'createTime': 1000, 'text': '二级引用'}},
                          {'text': '缺 ID 不建立来源', 'createTime': 1200}]},
            authorized=(500, 2000))  # child is outside incremental scan but within authorized week
        self.assertEqual(self.row('SELECT body,reply_to FROM messages'), ('这是我写的', None))
        self.assertEqual(self.conn.execute('''SELECT kind,source_cid,source_mid,body FROM message_relations
                ORDER BY path''').fetchall(), [('forward', 'c', 'f1', '仅转发'),
                ('quote', 'c', 'nested', '二级引用'), ('quote', 'other', 'original', '仅引用')])
        self.assertEqual(self.row("SELECT sender_id,message_type FROM message_relations WHERE path='quotedMessage'"),
                         ('me', 'text'))
        self.save({'quotedMessage': {'openMessageId': 'next', 'content': 'NO-TIME'}})
        self.assertEqual(self.row('SELECT reply_to FROM messages'), ('next',))
        self.assertEqual(self.row('SELECT count(*),body,raw_json FROM message_relations'), (1, None, None))

    def test_expired_unknown_and_future_bodies_and_signed_urls_are_not_persisted(self):
        for stamp in (None, 'invalid-date', 999, 2000, 2001):
            with self.subTest(stamp=stamp):
                child = {'messageId': 'expired', 'createTime': stamp, 'content': 'FORBIDDEN'}
                self.save({'text': 'FORBIDDEN (forward preview)', 'forwarded': [child], 'quotedMessage': child})
                self.assertNotIn('FORBIDDEN', '\n'.join(self.conn.iterdump()))
                self.assertEqual(self.row('SELECT count(*) FROM message_relations'), (2,))
        self.save({'text': 'FORBIDDEN', '_verified_detail': {'messageId': 'outer', 'createTime': 1500,
            'text': 'FORBIDDEN', 'forwarded': [{'messageId': 'expired', 'createTime': 999, 'text': 'FORBIDDEN'}]}})
        self.assertNotIn('FORBIDDEN', '\n'.join(self.conn.iterdump()))
        url = 'https://cdn.invalid/a?sig=PRIVATE'
        self.save({'text': json.dumps({'textContent': {'text': f'参考 {url}'},
            'attachment': {'downloadUrl': url}, 'quotedMessage': {'content': 'FORBIDDEN'}}),
                   'quotedMessage': {'openMessageId': 'q', 'createTime': 1000, 'content': f'参考 {url}'}})
        persisted = '\n'.join(self.conn.iterdump())
        for forbidden in ('PRIVATE', 'cdn.invalid', 'FORBIDDEN'):
            self.assertNotIn(forbidden, persisted)
        self.assertIn('参考', self.row('SELECT body FROM messages')[0])

    def test_verified_detail_preserves_index_representation_and_relations(self):
        detail = {'messageId': 'outer', 'conversationId': 'c', 'createTime': 1499, 'text': '补读正文', 'messageType': 'richText',
                  'forwarded': [{'conversationId': 'fc', 'messageId': 'f', 'text':'原文', 'createTime': 1100}],
                  'resourceRefs': [{'resourceId': 'detail-picture'}]}
        msg = {'openMessageId': 'open-alias', 'text': '[图片]', 'resourceRefs': [{'resourceId': 'index-picture'}],
               'quotedMessage': {'messageId': 'q'}, '_verified_detail': detail}
        self.save(msg)
        self.assertEqual(self.row('SELECT body,reply_to FROM messages'), ('补读正文', 'q'))
        self.assertEqual(self.row('SELECT occurred_at FROM messages'), (1500,))
        raw = json.loads(self.row('SELECT raw_json FROM message_capture')[0])
        self.assertEqual(raw, {'text': '[图片]', 'createTime': 1500, 'openMessageId': 'open-alias',
                               'detail': {'text': '补读正文', 'createTime': 1499}})
        self.assertEqual(self.conn.execute('SELECT source_mid FROM message_relations ORDER BY source_mid').fetchall(),
                         [('f',), ('q',)])
        media_message = self.media.enqueue.call_args.args[-1]
        self.assertEqual(media_message['resourceRefs'], msg['resourceRefs'])
        self.assertEqual(media_message['_verified_detail']['resourceRefs'], detail['resourceRefs'])
        self.save({'text': '已撤回'})
        self.assertEqual(self.row('SELECT body,reply_to FROM messages'), ('已撤回', None))
        self.assertEqual(self.row('SELECT count(*) FROM message_relations'), (0,))

    def run_cycle(self, end, pages, *, fail_drain=False):
        calls = []
        def api(_dws, *args, **_kwargs):
            self.assertEqual(args[:3], ('chat', 'message', 'list-all'))
            calls.append((args[args.index('--start')+1], args[args.index('--end')+1], args[-1]))
            response = next(pages)
            if isinstance(response, Exception):
                raise response
            return {'result': {'hasMore': bool(response), 'nextCursor': response, 'conversationMessagesList': []}}
        self.media.drain.side_effect = OSError if fail_drain else None
        with patch.object(sync, 'bind_profile', return_value='corp:me'), patch.object(sync, 'bind_self'), \
             patch.object(sync.importlib, 'import_module', return_value=self.media), \
             patch.object(sync, 'enrich_mentions'), patch.object(sync, 'dws_json', side_effect=api), \
             patch.object(sync.time, 'sleep'):
            if fail_drain:
                with self.assertRaisesRegex(sync.SyncError, 'media-drain'):
                    sync.sync(self.conn, Path('.'), 'unused', end)
            else:
                sync.sync(self.conn, Path('.'), 'unused', end)
        return calls

    def test_existing_archive_sweeps_seven_days_once_and_keeps_success_watermark(self):
        end = 1_795_000_000_000
        previous = end - sync.HOUR_MS
        sync.set_state(self.conn, 'last_success_ms', previous)
        calls = self.run_cycle(end, iter([None]), fail_drain=True)
        self.assertEqual(calls, [(sync.dws_time(end-sync.INITIAL_MS), sync.dws_time(end), '0')])
        self.assertEqual(sync.state(self.conn, 'last_success_ms'), str(previous))
        self.assertEqual(sync.state(self.conn, 'message_capture_version'), '1')
        self.assertEqual(self.run_cycle(end, iter([])), [])  # pending media stage, not another backfill
        self.assertEqual(sync.state(self.conn, 'last_success_ms'), str(end))
        calls = self.run_cycle(end+1000, iter([None]))
        self.assertEqual(calls[0][0], sync.dws_time(end-sync.OVERLAP_MS))

    def test_legacy_inflight_cursor_is_not_reused_by_or_lost_to_backfill(self):
        end = 1_795_000_000_000
        sync.set_state(self.conn, 'last_success_ms', end-sync.HOUR_MS)
        existing = sync.fixed_window(self.conn, end)
        existing['cursor'] = 'legacy-page'; encoded = json.dumps(existing)
        sync.set_state(self.conn, 'message_window', encoded)
        with self.assertRaisesRegex(sync.SyncError, 'synthetic failure'):
            self.run_cycle(end, iter(['capture-page', sync.SyncError('synthetic failure')]))
        self.assertEqual(sync.state(self.conn, 'message_window'), encoded)
        self.assertIsNone(sync.state(self.conn, 'message_capture_version'))
        self.assertEqual(sync.state(self.conn, 'last_success_ms'), str(end-sync.HOUR_MS))
        calls = self.run_cycle(end, iter([None, None]))
        self.assertEqual([call[2] for call in calls], ['capture-page', 'legacy-page'])
        self.assertEqual(sync.state(self.conn, 'message_capture_version'), '1')
        self.assertIsNone(sync.state(self.conn, 'message_capture_window'))


if __name__ == '__main__':
    unittest.main()
