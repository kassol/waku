#!/usr/bin/env python3
import json
import pathlib
import sys

if '--version' in sys.argv:
    print('Claude Code 0.0.0')
    sys.exit(0)

if '--mcp-config' in sys.argv:
    pathlib.Path('mcp-config.json').write_text(sys.argv[sys.argv.index('--mcp-config') + 1])
pathlib.Path('claude-args.json').write_text(json.dumps(sys.argv[1:]))

def send(value):
    print(json.dumps(value), flush=True)

for line in sys.stdin:
    value = json.loads(line)
    if value.get('type') == 'control_response':
        send({'type': 'result', 'is_error': False, 'stop_reason': 'end_turn'})
    if value.get('type') == 'user':
        content = ''.join(block.get('text', '') for block in value.get('message', {}).get('content', []))
        if content == 'wait for approval':
            send({'type': 'control_request', 'request_id': 'approval', 'request': {'subtype': 'can_use_tool', 'tool_name': 'Bash', 'input': {'command': 'echo approved'}, 'description': 'Approve fixture command'}})
            continue
        if content == 'wait for answer':
            send({'type': 'control_request', 'request_id': 'answer', 'request': {'subtype': 'can_use_tool', 'tool_name': 'AskUserQuestion', 'input': {'questions': [{'header': 'Choice', 'question': 'Continue?', 'options': [{'label': 'Yes', 'description': 'Continue'}], 'multiSelect': False}]}}})
            continue
        send({'type': 'stream_event', 'event': {'type': 'message_start', 'message': {'role': 'assistant'}}})
        send({'type': 'stream_event', 'event': {'type': 'content_block_delta', 'delta': {'type': 'text_delta', 'text': 'Claude child finished.'}}})
        send({'type': 'result', 'is_error': False, 'stop_reason': 'end_turn'})
