#!/usr/bin/env python3
import json, pathlib, sys
if '--version' in sys.argv:
    print('codex-cli 0.0.0'); sys.exit(0)
def send(value): print(json.dumps(value), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method, rid = request.get('method'), request.get('id')
    if rid is None: continue
    if method in ('thread/start', 'thread/resume'):
        send({'id': rid, 'result': {'thread': {'id':'input-thread'}}})
    elif method in ('turn/start', 'turn/steer'):
        with pathlib.Path('input-calls.jsonl').open('a') as f: f.write(json.dumps(request) + '\n')
        text = request['params']['input'][0]['text']
        if text == 'uncertain': continue
        if text == 'reject':
            send({'id': rid, 'error': {'code':-32000,'message':'explicit rejection'}}); continue
        send({'id':rid, 'result':{'turn':{'id':'native-turn'}}})
        if method == 'turn/start': send({'method':'turn/started','params':{'turn':{'id':'native-turn'}}})
    else: send({'id':rid,'result':{}})
