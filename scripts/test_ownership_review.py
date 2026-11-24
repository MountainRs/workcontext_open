"""Offline regressions for desktop account consent and reader-facing review cards."""
from contextlib import closing
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import ownership_review as review


class AccountConsentTest(unittest.TestCase):
    def test_wrong_archive_rejected_before_dws_or_collection(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'archive').mkdir()
            with closing(sqlite3.connect(root / 'archive' / 'archive.sqlite3')) as db, db:
                db.execute('CREATE TABLE archive_state(key TEXT PRIMARY KEY, value TEXT)')
                db.execute("INSERT INTO archive_state VALUES('profile','corp:other')")
            with patch.object(review.archive, 'dws_json') as dws, patch.object(review, 'collect') as collect:
                with self.assertRaisesRegex(ValueError, '归档账号与授权账号不一致'):
                    review.run(root, 'dws', 'offline', 'corp:me')
                dws.assert_not_called()
                collect.assert_not_called()

    def test_switch_blocks_model_submission_or_report_publication(self):
        for matching_checks in (1, 2):
            with self.subTest(matching_checks=matching_checks), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                (root / 'ownership').mkdir()
                report = root / 'ownership' / 'latest.json'
                report.write_text('previous snapshot', encoding='utf8')
                checks = [('archive', 'corp:me', 'alias')] * matching_checks + [ValueError('账号已切换')]
                with patch.object(review, 'read_bound', side_effect=checks) as binding, \
                     patch.object(review, 'collect', return_value=({}, {})), \
                     patch.object(review, 'model_call') as model:
                    with self.assertRaisesRegex(ValueError, '账号已切换'):
                        review.run(root, 'dws', 'offline', 'corp:me')
                    model.assert_not_called()
                    self.assertTrue(all(call.args == (root, 'dws', 'corp:me') for call in binding.call_args_list))
                self.assertEqual(report.read_text(encoding='utf8'), 'previous snapshot')


class ReaderCardsTest(unittest.TestCase):
    def setUp(self):
        self.action = '查交接记录，确认旧预约是否已转入新册'
        self.event = '值班台已启用新登记册，交接记录列有旧预约的转入情况。'
        self.item = {
            'work': '旧预约转入新册的状态', 'bucket': '本人需核实',
            'action': self.action, 'why': '本人受托查明旧预约的状态。', 'when': '',
            'evidence': [{'cid': 'test', 'time': '2025-01-10', 'excerpt': self.event + '请你核对。', 'fromMe': False, 'toMe': True}],
        }

    def response(self, event):
        return {'index': 0, 'card': {
            'proof': {'kind': 'assigned', 'lines': [1]},
            'work': self.item['work'], 'action': self.action, 'when': '',
            'sourceSummary': {'time': '2025-01-10', 'place': '', 'people': '值班台', 'event': event},
        }}

    def test_rewrites_repeated_action_without_losing_verification_item(self):
        duplicate = dict(self.item)
        batches = []

        def reply(system, payload):
            batches.append([entry['index'] for entry in payload['items']])
            if 'instruction' in payload:
                return [self.response(self.event)]
            return [{'index': 1, 'card': None}, self.response(self.action + '。')]

        cards = review.reader_cards([self.item, duplicate], '2025-01-11', reply)
        self.assertEqual(batches, [[0, 1], [0]])
        self.assertEqual(cards[0]['bucket'], '本人需核实')
        self.assertEqual(cards[0]['sourceSummary']['event'], self.event)

    def test_usable_action_survives_an_uninformative_second_summary(self):
        cards = review.reader_cards([self.item], '2025-01-11', lambda *_: [self.response(self.action)])
        self.assertEqual([c['action'] for c in cards], [self.action])

    def test_waiting_requires_a_request_from_me_or_a_promise_to_me(self):
        self.item['bucket'] = '等待他人'
        response = self.response(self.event)
        response['card']['proof'] = {'kind': 'promised', 'lines': [1]}
        self.item['evidence'][0]['toMe'] = False
        call = lambda *_: [response]
        self.assertEqual(review.reader_cards([self.item], '2025-01-11', call), [])
        self.item['evidence'][0]['fromMe'] = True
        response['card']['proof']['kind'] = 'requested'
        self.assertEqual(review.reader_cards([self.item], '2025-01-11', call)[0]['action'], self.action)

    def test_distinct_origin_never_receives_other_deliveries_notes_or_cards(self):
        old = dict(self.item, anchors=[{'cid': 'test', 'mid': 'old'}], conclusion='会议结论')
        new = dict(self.item, anchors=[{'cid': 'test', 'mid': 'new'}], conclusion=None)

        def reply(system, payload):
            self.assertNotIn('previous', payload)
            self.assertEqual(len(payload['items']), 1)
            entry = payload['items'][0]
            self.assertEqual(entry['confirmedContext'], ['会议结论'] if entry['index'] == 0 else [])
            return [dict(self.response(self.event), index=entry['index'])]

        cards = review.reader_cards([old, new], '2025-01-11', reply)
        self.assertEqual([c['itemIndex'] for c in cards], [0, 1])


class DeliveryConclusionTest(unittest.TestCase):
    def setUp(self):
        self.names = ['20份流程图', '会上已确认旧流程图', '20款手工线工序差异表', '另批手工线工序差异表']
        self.conversation = {'type': '私聊', 'confirmedContext': ['整组不用跟进'], 'messages': []}
        self.locations = {}
        for n, text in enumerate(self.names, 1):
            day = (10, 11, 20, 21)[n - 1]
            stamp = f'2026-11-{day}T10:00:00+08:00'
            self.conversation['messages'].append([n, stamp, '我', [], '', text])
            self.locations['room', n] = {'cid': 'room', 'mid': str(n), 'time': stamp, 'kind': '私聊',
                                         'conversation': '项目', 'excerpt': text, 'fromMe': True, 'toMe': False}
        self.decisions = [('room', {'originLines': [n], 'lines': [n, 2], 'owner': '他人', 'action': self.names[n - 1]})
                          for n in (1, 3, 4)]
        self.calls = []

    def model(self, system, payload):
        self.calls.append(payload)
        candidates = payload['candidates']
        # The reviewer tries to collapse every similar item and cites shared background only.
        return [{'ids': [c['id'] for c in candidates], 'evidence': ['C1/2'], 'bucket': '等待他人',
                 'work': candidates[0]['candidate']['action'], 'action': '等待交付', 'why': '本人已提出要求', 'when': ''}]

    def resolve(self, rules):
        return review.resolve({'room': self.conversation}, self.locations, self.decisions, '', self.model, rules)

    def test_project_requires_this_deliveries_cited_message(self):
        project = '白鹭迁移项目'
        for n in (1, 3, 4):
            location = self.locations['room', n]
            location['conversation'] = project
            if n in (3, 4):
                location['excerpt'] = project + '：' + location['excerpt']
                self.conversation['messages'][n - 1][-1] = location['excerpt']

        def model(system, payload):
            return [{'ids': [c['id']], 'evidence': [c['refs'][0]], 'project': project,
                     'bucket': '等待他人', 'work': c['candidate']['action'], 'action': '等待交付',
                     'why': '本人已提出要求', 'when': ''} for c in payload['candidates']]

        items = review.resolve({'room': self.conversation}, self.locations, self.decisions, '', model)
        self.assertEqual(sorted((i['anchors'][0]['mid'], i.get('project')) for i in items),
                         [('1', None), ('3', project), ('4', project)])

    def test_generic_and_internal_labels_in_cited_text_are_unassigned(self):
        for label in ('该项目', '相关工作', '同事3', '成员12项目', 'C1/4'):
            with self.subTest(label=label):
                self.assertIsNone(review.cited_project(label, [f'请核对{label}的清单']))

    def test_meeting_closes_only_original_delivery_despite_overlap_and_invalid_merge(self):
        rules = {frozenset({('room', '1')}): ('会上已明确无需再回复', True),
                 frozenset({('room', '2'), ('room', '3')}): ('共享背景不足以匹配', True)}
        items = self.resolve(rules)
        self.assertEqual({i['work'] for i in items}, set(self.names[2:]))
        self.assertTrue(any(len(p['candidates']) == 2 for p in self.calls))
        for item in items:
            mid = str(self.names.index(item['work']) + 1)
            self.assertEqual(item['anchors'], [{'cid': 'room', 'mid': mid}])
            self.assertEqual({e['mid'] for e in item['evidence']}, {'2', mid})
            self.assertTrue(all(e['kind'] == '私聊' for e in item['evidence']))
        self.assertTrue(all('confirmedContext' not in p for p in self.calls))

    def test_missing_schema_profile_binding_and_reopen(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.assertEqual(review.read_conclusions(root, 'me'), {})
            with closing(sqlite3.connect(root / 'workcontext.sqlite3')) as db:
                self.assertEqual(review.read_conclusions(root, 'me'), {})
                db.executescript('''CREATE TABLE ownership_card(id TEXT PRIMARY KEY, profile TEXT);
                    CREATE TABLE ownership_conclusion(card_id TEXT PRIMARY KEY,note TEXT,resolved INTEGER,anchors_json TEXT);
                    INSERT INTO ownership_card VALUES('old','me'),('other','other');
                    INSERT INTO ownership_conclusion VALUES('old','会议结论',1,'[{"cid":"room","mid":"1"}]'),
                    ('other','其他账号',1,'[{"cid":"room","mid":"3"}]');''')
                self.assertEqual(len(self.resolve(review.read_conclusions(root, 'me'))), 2)
                db.execute("UPDATE ownership_conclusion SET resolved=0 WHERE card_id='old'")
                db.commit()
                self.calls.clear()
                items = self.resolve(review.read_conclusions(root, 'me'))
        self.assertEqual({i['work']: i['conclusion'] for i in items},
                         {self.names[0]: '会议结论', self.names[2]: None, self.names[3]: None})
        for payload in self.calls:
            for c in payload['candidates']:
                if c['conclusion']:
                    self.assertEqual(len(payload['candidates']), 1)
                    self.assertEqual(c['candidate']['originLines'], [1])

    def test_legacy_feedback_is_only_an_inventory_lead(self):
        note = '会上已确认不用再回复'
        section = {'type': '私聊', 'messages': self.conversation['messages']}

        def extract(system, payload):
            if 'inventory' not in payload:
                self.assertEqual(payload['historicalLeads'], [note])
                return [{'action': self.names[n - 1], 'actors': '他人', 'lines': [n]} for n in (1, 3)]
            self.assertNotIn('historicalLeads', payload['conversation'])
            return [dict(item, owner='他人', state='待处理', basis='本人已提出要求', originLines=item['lines'])
                    for item in payload['inventory']]

        result = review.review_section(section, extract, [note])
        self.assertEqual([row['originLines'] for row in result['open']], [[1], [3]])
