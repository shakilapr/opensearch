import json, subprocess

req = {
    'jsonrpc': '2.0',
    'id': 13,
    'method': 'tools/call',
    'params': {
        'name': 'read_passages',
        'arguments': {'resource_id': 'res_b0c9d1d6e8454788878de487517748fb'}
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
print(out)
