import json, subprocess

req = {
    'jsonrpc': '2.0',
    'id': 20,
    'method': 'tools/call',
    'params': {
        'name': 'bridge_concepts',
        'arguments': {
            'concept_a': 'chemistry',
            'concept_b': 'romance',
            'top_k': 3,
            'mode': 'latent'
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
