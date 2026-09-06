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

turn = 0
for line in sys.stdin:
    request = json.loads(line)
    method, request_id = request.get('method'), request.get('id')
    if request_id is None:
        continue
    if method in ('thread/start', 'thread/resume'):
        send({'id': request_id, 'result': {'thread': {'id': 'fixture-thread'}}})
    elif method == 'turn/start':
        if any(item.get('text') == 'reject' for item in request['params']['input']):
            send({'id': request_id, 'error': {'code': -32000, 'message': 'fixture rejected'}})
            continue
        turn += 1
        with pathlib.Path('prompt-calls.jsonl').open('a') as calls:
            calls.write(json.dumps(request['params']) + '\n')
        send({'id': request_id, 'result': {'turn': {'id': str(turn)}}})
        send({'method': 'turn/started', 'params': {'turn': {'id': str(turn)}}})
        if turn > 1:
            send({'method': 'turn/completed', 'params': {'turn': {'id': str(turn - 1), 'status': 'interrupted'}}})
        # Keep the turn open until the test requests cancellation.
        pathlib.Path('prompt-ready').touch()
    elif method == 'turn/interrupt':
        send({'id': request_id, 'result': {}})
        pathlib.Path('cancel-received').touch()
        deadline = time.monotonic() + 10
        while not pathlib.Path('cancel-release').exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        send({'method': 'turn/completed', 'params': {'turn': {'id': str(turn), 'status': 'interrupted'}}})
    else:
        send({'id': request_id, 'result': {}})
