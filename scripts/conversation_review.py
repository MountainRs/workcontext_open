"""Review one frozen, authorized conversation using a caller-supplied model adapter."""
import json
from pathlib import Path


PROMPTS = Path(__file__).resolve().parents[1] / 'fixtures' / 'conversation_review_prompts.json'
OPEN = {'待处理', '需核实'}
STATES = OPEN | {'已结束', '无行动'}


def _check_rows(items, valid_lines, required_text):
    if not isinstance(items, list):
        raise ValueError('模型未返回事项列表')
    for item in items:
        if not isinstance(item, dict) or any(
            not isinstance(item.get(field), str) or not item[field].strip() for field in required_text
        ):
            raise ValueError('事项字段缺失')
        lines = item.get('lines')
        if not isinstance(lines, list) or not lines or any(
            type(line) is not int or line not in valid_lines for line in lines
        ):
            raise ValueError('事项证据行不属于本会话')


def _open_items(conversation, decisions):
    items = [row for row in decisions if row['state'] in OPEN]
    if conversation['type'] != '群聊':
        return items
    self_lines = {row[0] for row in conversation['messages'] if row[2] == '我' or '我' in row[3]}
    roles = conversation.get('confirmed_responsibilities', [])
    for index, row in enumerate(items):
        if row['owner'] != '我' or not self_lines.isdisjoint(row['lines']):
            continue
        role = row.get('responsibility')
        if type(role) is int and 1 <= role <= len(roles):
            continue
        items[index] = dict(row, owner='待定', owner_review='引用行缺少本人发言、可验证的个人提及或已确认分工')
    return items


def review(conversation, call_items):
    """call_items(system_prompt, payload) returns the decoded `items` list or raises."""
    messages = conversation['messages']
    line_ids = [row[0] for row in messages]
    if any(type(line) is not int for line in line_ids) or len(set(line_ids)) != len(line_ids):
        raise ValueError('输入行号必须是唯一整数')
    valid_lines = set(line_ids)
    prompts = json.loads(PROMPTS.read_text(encoding='utf-8'))
    inventory = call_items(prompts['inventory'], conversation)
    _check_rows(inventory, valid_lines, ('action', 'actors'))
    numbered = [dict(row, item=index) for index, row in enumerate(inventory, 1)]
    system = '\n'.join(prompts[key] for key in ('disposition', 'confirmed_roles', 'shared_work', 'all_actors'))
    if conversation['type'] == '群聊' and conversation.get('confirmed_responsibilities'):
        system += '\n群内若依据已确认的分工判给本人，须在该项用 responsibility 返回所用分工的序号（从 1 起）。'
    decisions = call_items(system, {'conversation': conversation, 'inventory': numbered})
    _check_rows(decisions, valid_lines, ('action', 'state', 'owner', 'basis'))
    remaining = set(range(1, len(numbered) + 1))
    for row in decisions:
        if 'item' not in row or row['state'] not in STATES or row['owner'] not in {'我', '他人', '待定'}:
            raise ValueError('事项处置回执无效')
        item_id = row['item']
        if item_id is None:
            continue
        if type(item_id) is not int or item_id not in remaining:
            raise ValueError('事项编号重复或不存在')
        remaining.remove(item_id)
    if remaining:
        raise ValueError('模型漏回底稿中的事项')
    return {'inventory': numbered, 'decisions': decisions, 'open': _open_items(conversation, decisions)}
