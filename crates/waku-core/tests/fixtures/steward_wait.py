#!/usr/bin/env python3
import json
import pathlib
import sys
import threading
import time

root = pathlib.Path(__file__).parent
is_parent = '--mcp-config' in sys.argv
if '--version' in sys.argv:
    print('codex-cli 0.0.0')
    sys.exit(0)

def send(value):
    print(json.dumps(value), flush=True)

def wait_file(name):
    deadline = time.monotonic() + 15
    while not root.joinpath(name).exists():
        if time.monotonic() > deadline:
            raise RuntimeError('fixture release timed out: ' + name)
        time.sleep(0.01)

if is_parent:
    pathlib.Path('mcp-config.json').write_text(sys.argv[sys.argv.index('--mcp-config') + 1])
    initial_input = threading.Event()
    def finish_parent():
        if not initial_input.wait(15):
            raise RuntimeError('initial parent prompt was not received')
        wait_file('finish-parent')
        send({'type': 'result', 'is_error': False, 'stop_reason': 'end_turn'})
    threading.Thread(target=finish_parent, daemon=True).start()
    for line in sys.stdin:
        value = json.loads(line)
        if value.get('type') == 'user':
            if not initial_input.is_set():
                initial_input.set()
                root.joinpath('parent-prompt-received').touch()
                continue
            with root.joinpath('callback-prompts.jsonl').open('a') as output:
                output.write(json.dumps(value) + '\n')
            send({'type': 'result', 'is_error': False, 'stop_reason': 'end_turn'})
else:
    for line in sys.stdin:
        value = json.loads(line)
        method = value.get('method')
        request_id = value.get('id')
        if request_id is None:
            continue
        if method in ('thread/start', 'thread/resume'):
            send({'id': request_id, 'result': {'thread': {'id': 'wait-child'}}})
        elif method == 'turn/start':
            send({'id': request_id, 'result': {'turn': {'id': 'wait-turn'}}})
            send({'method': 'turn/started', 'params': {'threadId': 'wait-child', 'turn': {'id': 'wait-turn'}}})
            def finish_child():
                wait_file('finish-child')
                event = {'method': 'turn/completed', 'params': {'threadId': 'wait-child', 'turn': {'id': 'wait-turn', 'status': 'completed'}}}
                send(event)
                wait_file('repeat-child')
                send(event)
                root.joinpath('repeat-sent').touch()
            threading.Thread(target=finish_child, daemon=True).start()
        else:
            send({'id': request_id, 'result': {}})
