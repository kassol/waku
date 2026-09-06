#!/usr/bin/env python3
"""Controlled provider: real Git checks and recorded RPCs, no model or network."""
import json
import pathlib
import subprocess
import sys
import threading
import time

root = pathlib.Path(__file__).parent
cwd = pathlib.Path.cwd()
key = cwd.name
lock = threading.Lock()

def send(value):
    with lock:
        print(json.dumps(value), flush=True)

def record(value):
    with root.joinpath(key + '.calls').open('a') as out:
        out.write(json.dumps(value) + '\n')

def git(*args):
    return subprocess.check_output(['git', *args], text=True).strip()

def check(commit, path, expected):
    actual = git('show', commit + ':' + path)
    assert actual == expected, actual
    record({'check': path, 'commit': commit, 'environment': 'controlled-git-v1'})

def commit(path, value):
    pathlib.Path(path).write_text(value + '\n')
    git('add', path)
    git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'Controlled result')
    return git('rev-parse', 'HEAD')

def await_release():
    deadline = time.monotonic() + 30
    while not root.joinpath(key + '.release').exists():
        if time.monotonic() > deadline:
            raise RuntimeError('fixture release timed out')
        time.sleep(.01)

def finish(turn, result):
    await_release()
    # Read the latest accepted result after any native feedback.
    result = root.joinpath(key + '.ready').read_text()
    send({'method': 'item/agentMessage/delta', 'params': {'threadId': key, 'turnId': turn, 'itemId': turn, 'delta': result}})
    send({'method': 'turn/completed', 'params': {'threadId': key, 'turn': {'id': turn, 'status': 'completed'}}})

if '--safe-mode' in sys.argv:
    assert '--strict-mcp-config' in sys.argv and '--no-session-persistence' in sys.argv
    assert sys.argv[sys.argv.index('--tools') + 1] == ''
    assert json.loads(sys.argv[sys.argv.index('--mcp-config') + 1]) == {'mcpServers': {}}
    context = json.loads(sys.argv[-1])
    assert context['records'][0]['summary']['session_id'] == context['source_session_id']
    assert len(context['records']) >= 3
    with root.joinpath('consultation-calls.jsonl').open('a') as output:
        output.write(json.dumps({'source': context['source_session_id'], 'question': context['question']}) + '\n')
    send({'type': 'system', 'subtype': 'init', 'tools': [], 'mcp_servers': []})
    send({'type': 'result', 'subtype': 'success', 'result': 'Discussed only; no task direction has been changed.'})
    sys.exit(0)

if '--version' in sys.argv:
    print('codex-cli 0.0.0')
    sys.exit(0)
turn = None
title_session = False
for line in sys.stdin:
    request = json.loads(line)
    method, rid = request.get('method'), request.get('id')
    if rid is None:
        continue
    if method == 'initialize':
        title_session = request.get('params', {}).get('clientInfo', {}).get('name') == 'waku-title'
        send({'id': rid, 'result': {}})
    elif method in ('thread/start', 'thread/resume'):
        send({'id': rid, 'result': {'thread': {'id': key}}})
    elif method == 'turn/start':
        if title_session:
            send({'id': rid, 'result': {'turn': {'id': 'title'}}})
            send({'method': 'item/completed', 'params': {'threadId': key, 'item': {'type': 'agentMessage', 'text': json.dumps({'title': 'Controlled task'})}}})
            send({'method': 'turn/completed', 'params': {'threadId': key, 'turn': {'id': 'title', 'status': 'completed'}}})
            continue
        record(request)
        assignment = json.loads(request['params']['input'][0]['text'])
        role = assignment['owner']
        assert assignment['scope'] and assignment['baseline'] and assignment['acceptance']
        turn = 'controlled-turn'
        send({'id': rid, 'result': {'turn': {'id': turn}}})
        send({'method': 'turn/started', 'params': {'threadId': key, 'turn': {'id': turn}}})
        if role == 'implementer':
            result = {'commit': commit('feature.txt', 'initial'), 'owner': role}
        elif role == 'reviewer':
            fixed = assignment['commit']
            check(fixed, 'feature.txt', assignment['expected'])
            result = {'commit': fixed, 'owner': role, 'findings': assignment.get('findings', [])}
        else:
            # Dependent work consumes the exact accepted integration baseline.
            assert git('rev-parse', 'HEAD') == assignment['baseline']
            expected = assignment.get('expected', 'dependent result')
            result = {'commit': commit('dependent.txt', expected), 'owner': role}
            check(result['commit'], 'dependent.txt', expected)
        root.joinpath(key + '.ready').write_text(json.dumps(result))
        threading.Thread(target=finish, args=(turn, result), daemon=True).start()
    elif method == 'turn/steer':
        record(request)
        assert request['params']['expectedTurnId'] == turn
        feedback = json.loads(request['params']['input'][0]['text'])
        assert feedback['findings'] == ['replace initial with accepted']
        assert git('rev-parse', 'HEAD') == feedback['commit']
        fixed = commit('feature.txt', 'accepted')
        check(fixed, 'feature.txt', 'accepted')
        root.joinpath(key + '.ready').write_text(json.dumps({'commit': fixed, 'owner': 'implementer'}))
        send({'id': rid, 'result': {'turnId': turn}})
    else:
        record(request)
        send({'id': rid, 'result': {}})
