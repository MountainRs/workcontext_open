import copy
import io
import json
import unittest
from unittest.mock import patch

from context_organize import decode, organize
from ownership_review import MODEL, model_call


class OrganizerContract(unittest.TestCase):
    def setUp(self):
        self.request = {
            'source': {'originRef': 'synthetic/input/1', 'body': '甲批演练已结束。原因仍待核实。'},
            'scopes': [{'id': 'p1', 'name': '模拟演练'}],
            'candidates': [{'id': 'r1', 'title': '甲批演练', 'scopeId': 'p1',
                            'delivery': '甲批', 'anchors': ['synthetic/input/1']}],
        }
        self.record = {'kind': 'event', 'title': '甲批演练', 'anchor': 'synthetic/input/1',
                       'content': '甲批演练已结束。', 'delivery': '甲批', 'scopeId': 'p1', 'targetId': 'r1',
                       'facets': {'facts': ['甲批演练已结束。'], 'inferences': [], 'openQuestions': ['原因是什么？']}}

    def run_records(self, records):
        return organize(self.request, lambda system, payload: {'records': records})['records']

    def test_allowed_attachment_and_no_action(self):
        self.assertEqual(self.run_records([self.record]), [self.record])
        self.request['source']['body'] = '周末愉快！'
        self.assertEqual(self.run_records([]), [])

    def test_reject_invalid_decisions(self):
        for patch in [{'targetId': 'other'}, {'scopeId': 'foreign'}, {'scopeId': None}, {'delivery': '乙批'},
                      {'anchor': 'other'}, {'confirmed': True}, {'kind': 'task'}, {'content': '原因已查明'},
                      {'content': '原因已查明', 'facets': {'facts': ['原因已查明'], 'inferences': [], 'openQuestions': []}}]:
            with self.subTest(patch=patch), self.assertRaises(ValueError):
                self.run_records([{**self.record, **patch}])
        with self.assertRaises(ValueError):
            self.run_records([self.record, self.record])
        self.request['candidates'][0]['anchors'] = ['synthetic/input/2']
        with self.assertRaises(ValueError):
            self.run_records([self.record])

    def test_cross_source_match_requires_a_single_corroborated_quoted_ticket(self):
        candidate = self.request['candidates'][0]
        candidate.update(scopeId=None, anchors=['synthetic/input/previous', 'INC-123'])
        self.record['scopeId'] = None
        for source, fact, accepted in [
            ('INC-123 已恢复。', 'INC-123 已恢复。', True),
            ('INC-123 与 INC-456 均已恢复。', 'INC-123', False),
            ('INC-123 已恢复。', '已恢复。', False),
            ('INC-1234 尚未恢复。', 'INC-123', False),
            ('OLD_INC-123 尚未恢复。', 'INC-123', False),
            ('INC-456 已恢复。', 'INC-456 已恢复。', False),
        ]:
            with self.subTest(source=source, fact=fact):
                self.request['source']['body'] = source
                self.record['content'] = fact
                self.record['facets']['facts'] = [fact]
                if accepted:
                    self.assertEqual(self.run_records([self.record]), [self.record])
                else:
                    with self.assertRaises(ValueError):
                        self.run_records([self.record])

    def test_validate_before_call(self):
        duplicate = copy.deepcopy(self.request['scopes'][0])
        self.request['scopes'].append(duplicate)
        with self.assertRaises(ValueError):
            organize(self.request, lambda *_: self.fail('submitted invalid data'))
        with self.assertRaises(ValueError):
            decode('{"records":[],"records":[]}')

    def test_native_reply_can_attach_only_current_source_facts(self):
        parent = {'originRef': 'synthetic/parent', 'body': '请复测甲批。'}
        self.request['source']['replyTo'] = parent
        self.request['candidates'][0]['anchors'] = [parent['originRef']]
        self.assertEqual(self.run_records([self.record]), [self.record])
        self.request['candidates'][0]['anchors'] = ['synthetic/unrelated']
        with self.assertRaises(ValueError):
            self.run_records([self.record])
        self.request['candidates'][0]['anchors'] = [parent['originRef']]
        self.record['content'] = parent['body']
        self.record['facets']['facts'] = [parent['body']]
        with self.assertRaises(ValueError):
            self.run_records([self.record])
        self.request['source']['body'] = self.record['content'] = '收到。'
        self.record['facets']['facts'] = ['收到。']
        with self.assertRaises(ValueError):
            self.run_records([self.record])
        self.assertEqual(self.run_records([]), [])
        parent['body'] = 'x' * 4097
        with self.assertRaises(ValueError):
            self.run_records([])

    def test_previous_can_clarify_subject_but_cannot_supply_fact_or_merge_anchor(self):
        previous = {'originRef': 'synthetic/previous', 'body': '复核样品的误差范围'}
        self.request['source']['previous'] = previous
        self.request['candidates'][0]['anchors'] = [previous['originRef']]
        with self.assertRaises(ValueError):
            self.run_records([self.record])
        self.record['targetId'] = None
        self.assertEqual(self.run_records([self.record]), [self.record])
        self.record['content'] = previous['body']
        self.record['facets']['facts'] = [previous['body']]
        with self.assertRaises(ValueError):
            self.run_records([self.record])
        self.request['source']['replyTo'] = previous
        with self.assertRaises(ValueError):
            self.run_records([])

    def test_gateway_response_model(self):
        for identity in [{}, {'model': 'another-model'}, {'model': MODEL}]:
            envelope = {**identity, 'choices': [{'message': {'content': '{"records":[]}'}}]}
            with self.subTest(identity=identity), patch('urllib.request.build_opener') as opener:
                opener.return_value.open.return_value = io.BytesIO(json.dumps(envelope).encode())
                if identity.get('model') == MODEL:
                    self.assertEqual(model_call('fixture', '', {}, decode=decode, attempts=1, verify_model=True), {'records': []})
                else:
                    with self.assertRaises(ValueError):
                        model_call('fixture', '', {}, decode=decode, attempts=1, verify_model=True)
                opener.return_value.open.assert_called_once()

    def test_transport_failure_propagates(self):
        def unavailable(*_):
            raise TimeoutError()
        with self.assertRaises(TimeoutError):
            organize(self.request, unavailable)


if __name__ == '__main__':
    unittest.main()
