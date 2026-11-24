"""Synthetic message projections only; no DWS executable or archive is required."""
import hashlib
import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import archive_media as media


def reference(rid, kind='mediaId', **fields):
    return {'resourceIdType': kind, 'resourceId': rid, **fields}


class MediaCaptureTest(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        self.addCleanup(self.db.close)
        media.init(self.db)

    def capture(self, message):
        media.enqueue(self.db, 'chat', 'parent', message)

    def evidence(self):
        return self.db.execute('''SELECT o.path,o.source_cid,o.source_mid,a.resource_id,o.reason
            FROM attachment_occurrences o LEFT JOIN attachments a ON a.id=o.attachment_id
            WHERE o.cid='chat' AND o.mid='parent' ORDER BY o.path''').fetchall()

    def test_union_preserves_locations_and_merges_duplicate_work(self):
        message = {'resourceRefs': [reference('a'), reference('doc', 'fileId')],
                   'resources': [reference('a', name='photo.png'), reference('b')]}
        self.capture(message)
        self.capture(message)
        self.assertEqual([row[3] for row in self.evidence()], ['a', 'doc', 'a', 'b'])
        self.assertEqual(self.db.execute('SELECT resource_id,name FROM attachments ORDER BY resource_id').fetchall(),
                         [('a', 'photo.png'), ('b', None), ('doc', None)])

    def test_nested_refs_keep_parent_association_and_child_scope(self):
        self.capture({'forwarded': {'conversationId': 'other', 'messages': [
            {'openMessageId': 'child', 'resources': [reference('report', 'fileId')],
             'quotedMessage': {'messageId': 'source', 'resourceRefs': [reference('photo')]}},
            {'resourceRefs': [reference('no-source')]}]}})
        self.assertEqual(self.evidence(), [
            ('/forwarded/messages/0/quotedMessage/resourceRefs/0', 'other', 'source', 'photo', None),
            ('/forwarded/messages/0/resources/0', 'other', 'child', 'report', None),
            ('/forwarded/messages/1/resourceRefs/0', 'other', '', None, 'missing_media_message_context')])
        self.assertEqual(self.db.execute('SELECT mid,download_cid,download_mid FROM attachments WHERE resource_id=?',
                                         ('photo',)).fetchone(), ('parent', 'other', 'source'))

    def test_malformed_refs_and_placeholders_do_not_schedule_downloads(self):
        self.capture({'text': '[图片]', 'resourceRefs': {'url': 'not-an-id'}, 'resources': [
            {'url': 'not-an-id', 'downloadCommand': 'not an executable input'},
            reference('real', download={'arguments': {'resource-id': 'different'}}),
            reference('real', download={'missing': ['message-id']})]})
        self.assertEqual([r[4] for r in self.evidence()], ['image_placeholder_without_reference',
                         'malformed_reference_list', 'unverified_resource_id', 'conflicting_download_reference',
                         'missing_media_message_context'])
        self.assertEqual(self.db.execute('SELECT COUNT(*) FROM attachments').fetchone(), (0,))

    def test_verified_detail_resolves_typed_image_gap_and_preserves_sibling(self):
        index = {'text': json.dumps({'contentType': 2, 'textContent': {'text': ''}}),
                 'forwarded': [{'messageId': 'sibling', 'content': '[图片]'}]}
        self.capture(index)
        self.assertEqual([r[4] for r in self.evidence()], ['image_without_reference', 'image_placeholder_without_reference'])
        self.capture({**index, '_verified_detail': {'messageId': 'parent', 'conversationId': 'chat',
                                                    'resourceRefs': [reference('picture')]}})
        self.capture(index)
        self.assertEqual(self.evidence(), [('/_verified_detail/resourceRefs/0', 'chat', 'parent', 'picture', None),
                                          ('/forwarded/0', 'chat', 'sibling', None, 'image_placeholder_without_reference')])

    def test_unrelated_detail_cannot_authorize_media(self):
        self.capture({'_verified_detail': {'messageId': 'elsewhere', 'resourceRefs': [reference('never')]}})
        self.assertEqual(self.evidence(), [('/_verified_detail', 'chat', 'parent', None, 'detail_identity_mismatch')])

    def test_original_bytes_survive_unreadable_image(self):
        self.db.execute('CREATE TABLE messages(cid TEXT,mid TEXT,occurred_at INTEGER)')
        self.db.execute("INSERT INTO messages VALUES ('chat','parent',2)")
        self.capture({'resourceRefs': [reference('photo', name='photo.png')]})
        self.db.commit()
        original = b'synthetic image fixture; no readable text'
        digest = hashlib.sha256(original).hexdigest()

        def download(_dws, _profile, scope, stage):
            self.assertEqual(scope, ('mediaId', 'photo', 'chat', 'parent'))
            path = stage / 'asset.bin'
            path.write_bytes(original)
            return path, digest, len(original)

        with tempfile.TemporaryDirectory() as root, patch.object(media, '_download', download), patch.object(
                media, '_extract', return_value=(None, 'unsupported', None, 1, 0)):
            counts = media.drain(self.db, root, sys.executable, 'synthetic', 2, 3)
            status, text, local_path = self.db.execute('SELECT status,text_content,local_path FROM attachments').fetchone()
            self.assertEqual((status, text, counts['extraction_unsupported']), ('downloaded', None, 1))
            self.assertEqual((Path(root) / local_path).read_bytes(), original)



if __name__ == '__main__':
    unittest.main()
