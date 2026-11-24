"""Synthetic archive regressions; never launch DWS or inspect a user's archive."""
from contextlib import closing
import importlib.util
import os
import sqlite3
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

import archive_media as media
import archive_sync as sync


class ResumeTest(unittest.TestCase):
    def test_feed_bootstrap_replay_and_late_extraction(self):
        end = 1_795_000_000_000
        with closing(sqlite3.connect(':memory:')) as conn:
            conn.executescript(sync.SCHEMA)
            media.init(conn)
            window = {'start_ms': end - 1000, 'end_ms': end}
            msg = {'messageId': 'a', 'createTime': end - 1, 'text': 'original',
                   'resourceRefs': [{'resourceIdType': 'fileId', 'resourceId': 'file'}]}
            groups = [{'openConversationId': 'chat', 'messages': [msg, dict(msg, messageId='b')]}]
            def replay():
                sync.save_messages(conn, groups, window, media, None, None)
            def revisions():
                return dict(conn.execute('SELECT mid,seq FROM organizer_changes'))
            replay()
            sync.init_change_feed(conn, bootstrap_limit=1)
            self.assertEqual(set(revisions()), {'a'})
            sync.init_change_feed(conn, bootstrap_limit=1)
            initial = revisions()
            self.assertEqual(set(initial), {'a','b'})
            replay()
            self.assertEqual(revisions(), initial)
            msg['text'] = 'edited'
            replay()
            edited = revisions()
            self.assertGreater(edited['a'], max(initial.values()))
            self.assertEqual(edited['b'], initial['b'])
            conn.execute("UPDATE attachments SET status='downloaded',extraction_status='extracted',text_content='excerpt' WHERE mid='b'")
            self.assertGreater(revisions()['b'], edited['a'])
            self.assertEqual(revisions()['a'], edited['a'])
            reply = groups[0]['messages'][1]
            reply['quotedMessage'] = {'messageId': 'a', 'createTime': end - 2}
            replay()
            checkpoint = max(revisions().values())
            msg['text'] = 'corrected parent'
            replay()
            changed = conn.execute('SELECT mid,seq FROM organizer_changes WHERE seq>? ORDER BY seq', (checkpoint,)).fetchall()
            self.assertEqual({mid for mid, _ in changed}, {'a', 'b'})
            self.assertLess(changed[0][1], changed[1][1])
            self.assertEqual(conn.execute('SELECT mid,seq FROM organizer_changes WHERE seq>? ORDER BY seq',
                                          (changed[0][1],)).fetchall(), changed[1:])
            replay()
            self.assertEqual(revisions(), dict(changed))

    def test_out_of_order_predecessor_requeues_only_immediate_successor(self):
        with closing(sqlite3.connect(':memory:')) as conn:
            conn.executescript(sync.SCHEMA)
            media.init(conn)
            sync.init_change_feed(conn)
            def insert(mid, time):
                conn.execute('INSERT INTO messages(cid,mid,body,occurred_at) VALUES(?,?,?,?)', ('room',mid,mid,time))
            def revisions():
                return dict(conn.execute('SELECT mid,seq FROM organizer_changes'))
            insert('later', 3000)
            insert('last', 4000)
            before = revisions()
            insert('early', 2000)
            after = revisions()
            self.assertGreater(after['later'], before['later'])
            self.assertEqual(after['last'], before['last'])
            conn.execute("INSERT INTO message_capture(cid,mid,raw_json) VALUES('room','early','{}')")
            captured = revisions()
            self.assertGreater(captured['later'], after['later'])
            self.assertEqual(captured['last'], before['last'])
            conn.execute("DELETE FROM messages WHERE mid='early'")
            self.assertGreater(revisions()['later'], after['later'])

    def test_scheduled_capture_requires_existing_matching_binding(self):
        reply = {'currentProfile':'org:u', 'profiles':[{'profile':'org:u','corpId':'org','userId':'u',
                                                       'isCurrent':True,'isOrgCurrent':True}]}
        with closing(sqlite3.connect(':memory:')) as conn, patch.object(sync, 'dws_json', return_value=reply):
            conn.executescript(sync.SCHEMA)
            with self.assertRaises(sync.SyncError):
                sync.bind_profile(conn, 'dws', 'org:u')
            self.assertIsNone(sync.state(conn, 'profile'))
            sync.set_state(conn, 'profile', 'org:other')
            with self.assertRaises(sync.SyncError):
                sync.bind_profile(conn, 'dws', 'org:other')
            sync.set_state(conn, 'profile', 'org:u')
            self.assertEqual(sync.bind_profile(conn, 'dws', 'org:u'), 'org:u')

    def test_resume_after_failed_second_page(self):
        with tempfile.TemporaryDirectory() as temp, closing(sqlite3.connect(Path(temp) / 'archive.sqlite3')) as second:
            path = Path(temp) / 'archive.sqlite3'
            end = 1_795_000_000_000
            def page(groups, more):
                return {'result': {'conversationMessagesList': [
                    {'openConversationId': cid, 'messages': [
                        {'messageId': 'shared', 'createTime': end - 1000, 'senderId': 'u',
                         'text': body, 'quotedMessage': {'openMessageId': 'previous'}}]}
                    for cid, body in groups], 'hasMore': more, 'nextCursor': 'page-2'}}
            first = sqlite3.connect(path)
            first.executescript(sync.SCHEMA)
            media.init(first)
            # An incremental window can survive a later invocation without
            # crossing its seven-day authorization boundary.
            sync.set_state(first, 'last_success_ms', end - sync.HOUR_MS)
            window = sync.fixed_window(first, end)
            with patch.object(sync, 'dws_json', side_effect=[page([('a', 'old')], True), sync.SyncError('timeout')]), self.assertRaises(sync.SyncError):
                sync.sync_messages(first, 'dws', 'corp:me', window, media)
            first.close()
            resumed = sync.fixed_window(second, end + sync.HOUR_MS)
            self.assertEqual((resumed['cursor'], resumed['start_ms'], resumed['end_ms']),
                             ('page-2', end - 7 * sync.HOUR_MS, end))
            self.assertEqual(sync.state(second, 'last_success_ms'), str(end - sync.HOUR_MS))
            with patch.object(sync, 'dws_json', return_value=page([('a', 'edited'), ('b', 'distinct')], False)):
                sync.sync_messages(second, 'dws', 'corp:me', resumed, media)
            self.assertEqual(second.execute('SELECT cid,mid,body,reply_to FROM messages ORDER BY cid').fetchall(),
                             [('a','shared','edited','previous'),('b','shared','distinct','previous')])
            self.assertIsNone(sync.fixed_window(second,end)['cursor'])

    def test_stale_checkpoint_and_long_gap_stay_inside_seven_days(self):
        end = 1_795_000_000_000
        with closing(sqlite3.connect(':memory:')) as conn:
            conn.executescript(sync.SCHEMA)
            original = sync.fixed_window(conn, end)
            sync.set_state(conn, 'last_success_ms', end - 2 * sync.INITIAL_MS)
            resumed = sync.fixed_window(conn, end + 1)
            self.assertEqual(resumed['cursor'], '0')
            self.assertEqual(resumed['start_ms'], original['start_ms'] + 1000)
            self.assertEqual(resumed['end_ms'], end)

    def test_message_watermark_survives_interrupted_media_drain(self):
        end = 1_795_000_000_000
        profile = 'corp:me'
        replies = [
            {'currentProfile': profile, 'profiles': [{'profile': profile, 'corpId': 'corp', 'userId': 'me',
                                                     'isOrgCurrent': True, 'isCurrent': True}]},
            {'result': [{'orgEmployeeModel': {'userId': 'me', 'openDingTalkId': 'alias'}}]},
            {'result': {'hasMore': False, 'conversationMessagesList': []}},
            {'complete': True, 'hasMore': False, 'messages': []},
        ]
        with tempfile.TemporaryDirectory() as temp, closing(sqlite3.connect(':memory:')) as conn:
            conn.executescript(sync.SCHEMA)
            with patch.object(sync, 'dws_json', side_effect=replies), patch.object(media, 'drain', side_effect=OSError), self.assertRaises(sync.SyncError):
                sync.sync(conn, Path(temp), 'dws', end)
            self.assertEqual(sync.state(conn, 'messages_ready_ms'), str(end))
            self.assertIsNone(sync.state(conn, 'last_success_ms'))


    def test_sender_alias_is_profile_scoped_and_never_matched_by_name(self):
        with closing(sqlite3.connect(':memory:')) as conn:
            conn.executescript(sync.SCHEMA)
            media.init(conn)
            conn.execute("INSERT INTO messages(cid,mid,sender_id,occurred_at) VALUES('c','old','alias',1)")
            self_result = {'result': [{'orgEmployeeModel': {'corpId': 'corp', 'userId': 'me', 'openDingTalkId': 'alias'}},
                                      {'orgEmployeeModel': {'corpId': 'other', 'userId': 'me', 'openDingTalkId': 'wrong'}}]}
            with patch.object(sync, 'dws_json', return_value=self_result) as lookup:
                sync.bind_self(conn, 'dws', 'corp:me')
                lookup.assert_called_once_with('dws', 'contact', 'user', 'get-self', profile='corp:me')
            window = {'start_ms': 0, 'end_ms': 1000, 'cursor': '0'}
            page = {'result': {'hasMore': False, 'conversationMessagesList': [{'openConversationId': 'c',
                'messages': [{'messageId': s, 'senderId': s, 'sender': 'same name', 'createTime': 1}
                             for s in ('alias', 'other')]}]}}
            with patch.object(sync, 'dws_json', return_value=page):
                sync.sync_messages(conn, 'dws', 'corp:me', window, media)
            self.assertEqual(sync.state(conn, 'self_open_id'), 'alias')
            self.assertEqual(conn.execute('SELECT mid,sender_id FROM messages ORDER BY mid').fetchall(),
                             [('alias', 'me'), ('old', 'me'), ('other', 'other')])

    def test_expired_messages_and_quoted_media_are_not_fetched(self):
        with tempfile.TemporaryDirectory() as temp, closing(sqlite3.connect(':memory:')) as conn:
            conn.executescript(sync.SCHEMA)
            media.init(conn)
            conn.executemany('INSERT INTO messages(cid,mid,occurred_at) VALUES(?,?,?)',
                             [('c', 'expired', 1), ('c', 'recent', 2)])
            ref = {'type': 'mediaId', 'resourceId': 'picture'}
            media.enqueue(conn, 'c', 'expired', {'resourceRefs': [ref]})
            media.enqueue(conn, 'c', 'recent', {'quotedMessage': {'messageId': 'expired', 'resourceRefs': [ref]}})
            conn.commit()
            with patch.object(media, '_download', side_effect=AssertionError('out-of-window network read')):
                media.drain(conn, Path(temp), sys.executable, 'test:me', 2, 3)
            self.assertEqual(conn.execute('SELECT mid,status,failure_reason FROM attachments ORDER BY mid').fetchall(),
                             [('expired', 'pending', None), ('recent', 'failed', 'download:source_outside_window')])


class NativeReaderTest(unittest.TestCase):
    @unittest.skipUnless(os.name == 'nt', 'Windows redirected subprocess handles')
    def test_hidden_download_worker_preserves_receipt(self):
        with tempfile.TemporaryDirectory() as temp:
            args = [sys.executable, '-I', '-B', str(media._HELPER), 'limit', '65536',
                    sys.executable, '-c', 'print(\'{"sizeBytes":12}\')']
            rc, stdout, _ = media._run(args, Path(temp), 10, 65536)
            self.assertEqual((rc, media._json(stdout)), (0, {'sizeBytes': 12}))

    @unittest.skipUnless(importlib.util.find_spec('pypdf'), 'archive PDF reader not installed')
    def test_pdf_text_in_isolated_worker(self):
        from pypdf import PdfWriter
        from pypdf.generic import DecodedStreamObject, DictionaryObject, NameObject

        with tempfile.TemporaryDirectory() as temp:
            stage = Path(temp)
            pdf = stage / 'sample.pdf'
            writer = PdfWriter()
            page = writer.add_blank_page(200, 200)
            font = DictionaryObject({NameObject('/Type'): NameObject('/Font'), NameObject('/Subtype'): NameObject('/Type1'),
                                     NameObject('/BaseFont'): NameObject('/Helvetica')})
            page[NameObject('/Resources')] = DictionaryObject({NameObject('/Font'): DictionaryObject({NameObject('/F1'): font})})
            content = DecodedStreamObject()
            content.set_data(b'BT /F1 12 Tf 10 100 Td (Local PDF) Tj ET')
            page[NameObject('/Contents')] = content
            with pdf.open('wb') as out:
                writer.write(out)
            text, status, reason, *_ = media._extract(pdf, 'sample.pdf', stage, 0)
            self.assertEqual((status, reason), ('extracted', None))
            self.assertIn('Local PDF', text)

    def test_ooxml_worker_and_blob_verification(self):
        with tempfile.TemporaryDirectory() as temp:
            store = Path(temp) / 'attachments'
            store.mkdir(mode=0o700)
            with tempfile.TemporaryDirectory(dir=store) as scratch:
                stage = Path(scratch)
                asset = stage / 'asset.bin'
                with zipfile.ZipFile(asset, 'w') as office:
                    office.writestr('word/document.xml', '<doc><p><t>合成交付</t></p></doc>')
                text, status, reason, *_ = media._extract(asset, 'test.docx', stage, 0)
                self.assertEqual((text, status, reason), ('合成交付\n', 'extracted', None))
                digest, size = media._hash(asset)
                asset.replace(store / digest)
                with media._store_lock(store) as directory:
                    self.assertTrue(media._verified(directory, digest, size))

    @unittest.skipUnless(os.name == 'nt', 'requires inbox Windows OCR')
    def test_windows_ocr_reads_synthetic_image(self):
        with tempfile.TemporaryDirectory() as temp:
            stage = Path(temp)
            asset = stage / 'asset.png'
            powershell = Path(os.environ['SystemRoot']) / 'System32/WindowsPowerShell/v1.0/powershell.exe'
            draw = '''$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$bmp = New-Object Drawing.Bitmap(480,100)
$g = [Drawing.Graphics]::FromImage($bmp)
$g.Clear([Drawing.Color]::White)
$font = New-Object Drawing.Font('Arial',36)
$g.DrawString('HELLO 123', $font, [Drawing.Brushes]::Black, 10, 10)
$bmp.Save($env:WORKCONTEXT_TEST_IMAGE, [Drawing.Imaging.ImageFormat]::Png)
'''
            subprocess.run([str(powershell), '-NoProfile', '-NonInteractive', '-Command', draw],
                           check=True, env={**os.environ, 'WORKCONTEXT_TEST_IMAGE': str(asset)})
            text, status, reason, *_ = media._extract(asset, 'test.png', stage, 0)
            if reason == 'missing:windows_ocr_language':
                self.skipTest('no Windows OCR language installed')
            self.assertEqual((status, reason), ('extracted', None))
            self.assertIn('123', text)


if __name__ == '__main__':
    unittest.main()
