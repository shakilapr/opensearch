import json, subprocess

req = {
    'jsonrpc': '2.0',
    'id': 21,
    'method': 'tools/call',
    'params': {
        'name': 'find_documents_by_latent',
        'arguments': {
            'query': 'biochemical reactions in attachment and love',
            'limit': 3
        }
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
