#!/usr/bin/env python3
import json
import pathlib
import sys
import threading
import time

root = pathlib.Path(__file__).parent
is_parent = '--mcp-config' in sys.argv
is_codex_parent = pathlib.Path(__file__).name == 'codex-parent-fixture'
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
            if 'Waku explicit user instruction from independent discussion' in json.dumps(value):
                with root.joinpath('direction-prompts.jsonl').open('a') as output:
                    output.write(json.dumps(value) + '\n')
                def finish_direction():
                    wait_file('finish-direction')
                    send({'type': 'result', 'is_error': False, 'stop_reason': 'end_turn'})
                threading.Thread(target=finish_direction, daemon=True).start()
                continue
            with root.joinpath('callback-prompts.jsonl').open('a') as output:
                output.write(json.dumps(value) + '\n')
            send({'type': 'result', 'is_error': False, 'stop_reason': 'end_turn'})
else:
    thread_id = 'wait-parent' if is_codex_parent else 'wait-child'
    turn_count = 0
    if is_codex_parent:
        pathlib.Path('codex-args.json').write_text(json.dumps(sys.argv[1:]))
    for line in sys.stdin:
        value = json.loads(line)
        method = value.get('method')
        request_id = value.get('id')
        if request_id is None:
            continue
        if method in ('thread/start', 'thread/resume'):
            send({'id': request_id, 'result': {'thread': {'id': thread_id}}})
        elif method == 'turn/start':
            turn_count += 1
            turn_id = 'wait-turn-' + str(turn_count)
            send({'id': request_id, 'result': {'turn': {'id': turn_id}}})
            send({'method': 'turn/started', 'params': {'threadId': thread_id, 'turn': {'id': turn_id}}})
            if is_codex_parent:
                if turn_count == 1:
                    root.joinpath('parent-prompt-received').touch()
                    def finish_codex_parent(initial_turn=turn_id):
                        wait_file('finish-parent')
                        send({'method': 'turn/completed', 'params': {'threadId': thread_id, 'turn': {'id': initial_turn, 'status': 'completed'}}})
                    threading.Thread(target=finish_codex_parent, daemon=True).start()
                else:
                    with root.joinpath('callback-prompts.jsonl').open('a') as output:
                        output.write(json.dumps(value) + '\n')
                    send({'method': 'turn/completed', 'params': {'threadId': thread_id, 'turn': {'id': turn_id, 'status': 'completed'}}})
                continue
            def finish_child():
                wait_file('finish-child')
                event = {'method': 'turn/completed', 'params': {'threadId': thread_id, 'turn': {'id': turn_id, 'status': 'completed'}}}
                send(event)
                wait_file('repeat-child')
                send(event)
                root.joinpath('repeat-sent').touch()
            threading.Thread(target=finish_child, daemon=True).start()
        else:
            send({'id': request_id, 'result': {}})
