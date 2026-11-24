import sqlite3
import unittest

from capture_details import hydrate_page


class DetailReceiptsTest(unittest.TestCase):
    def test_only_matching_scope_and_actor_are_enriched(self):
        start = 1_790_000_000_000
        messages = [dict(conversationId='a', messageId=k, openMessageId=k, createTime=start+30_000,
                         senderId='me', sendType='unknown') for k in ('ok','actor','scope','expired','missing')]
        def dws(*args, **kwargs):
            details = [dict(messageId=k, conversationId=cid, senderId=who, createTime=stamp, text='source')
                       for k,cid,who,stamp in [('ok','a','me',start+20_000),('actor','a','other',start+30_000),
                                               ('scope','b','me',start+30_000),('expired','a','me',start-1000)]]
            return dict(messages=details, hasMore=False, requestedCount=5, notFoundMessageIds=['missing'])
        with sqlite3.connect(':memory:') as db:
            hydrate_page(db,'dws','org:me',dict(start_ms=start,end_ms=start+60_000),
                         [dict(openConversationId='a',messages=messages)], dws)
            self.assertEqual([m['messageId'] for m in messages if '_verified_detail' in m], ['ok'])
            self.assertEqual(dict(db.execute('select mid,status from message_detail_receipts')), {
                'ok':'verified_with_time_difference','actor':'unverified_identity_or_time','scope':'unverified_identity_or_time',
                'expired':'unverified_identity_or_time','missing':'not_found'})
