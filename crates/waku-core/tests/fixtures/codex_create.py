#!/usr/bin/env python3
import json
import pathlib
import sys
import time

if '--version' in sys.argv:
    print('codex-cli 0.0.0')
    sys.exit(0)

def send(value):
    print(json.dumps(value), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    request_id = request.get('id')
    if request_id is None:
        continue
    if method in ('thread/start', 'thread/resume'):
        send({'id': request_id, 'result': {'thread': {'id': 'fixture-thread'}}})
    elif method == 'turn/start':
        prompt = ''.join(item.get('text', '') for item in request['params']['input'])
        if prompt == 'reject fixture turn':
            send({'id': request_id, 'error': {'code': -32000, 'message': 'fixture rejected'}})
            continue
        if prompt == 'hold fixture turn':
            pathlib.Path('creation-waiting').touch()
            deadline = time.monotonic() + 15
            while not pathlib.Path('creation-release').exists() and time.monotonic() < deadline:
                time.sleep(0.01)
        if prompt in ('write fixture result', 'hold fixture turn'):
            pathlib.Path('child-result.txt').write_text('created in isolated worktree\n')
            with pathlib.Path('child-calls.jsonl').open('a') as calls:
                calls.write(json.dumps(request['params']) + '\n')
        send({'id': request_id, 'result': {'turn': {'id': 'fixture-turn'}}})
        send({'method': 'turn/started', 'params': {'turn': {'id': 'fixture-turn'}}})
        send({'method': 'item/agentMessage/delta', 'params': {'delta': 'Child finished.'}})
        send({'method': 'turn/completed', 'params': {'turn': {'id': 'fixture-turn', 'status': 'completed'}}})
    else:
        send({'id': request_id, 'result': {}})
