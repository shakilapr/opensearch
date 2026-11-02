import json, subprocess

req = {
    'jsonrpc': '2.0',
    'id': 12,
    'method': 'tools/call',
    'params': {
        'name': 'find_documents_by_concepts',
        'arguments': {'concepts': ['oxytocin synthesis', 'romantic love stages'], 'limit': 5}
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
