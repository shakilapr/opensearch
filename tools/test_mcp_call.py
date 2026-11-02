import json, subprocess

req = {
    'jsonrpc': '2.0',
    'id': 2,
    'method': 'tools/call',
    'params': {
        'name': 'probe_concepts',
        'arguments': {'query': 'diffusion', 'limit': 5}
    }
}
p = subprocess.Popen(
    ['/root/projects/search/.venv/bin/python', '/root/projects/search/tools/concepts/mcp_server.py'],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True
)
out, err = p.communicate(json.dumps(req) + '\n')
print('STDOUT:', out)
