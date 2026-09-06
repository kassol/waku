#!/usr/bin/env python3
import json, pathlib, sys
if '--version' in sys.argv:
    print('Claude Code 0.0.0'); sys.exit(0)
for line in sys.stdin:
    value = json.loads(line)
    if value.get('type') == 'user':
        with pathlib.Path('claude-input-calls.jsonl').open('a') as f:
            f.write(json.dumps(value) + '\n')
