import re

config_path = '/root/.hermes/config.yaml'
with open(config_path, 'r', encoding='utf-8') as f:
    content = f.read()

concepts_entry = '''mcp_servers:
  concepts:
    command: /root/projects/search/.venv/bin/python
    args:
    - /root/projects/search/tools/concepts/mcp_server.py
    enabled: true'''

if 'concepts:' not in content:
    new_content = content.replace('mcp_servers:', concepts_entry, 1)
    with open(config_path, 'w', encoding='utf-8') as f:
        f.write(new_content)
    print('Added concepts MCP server to config.yaml')
else:
    print('concepts already present in config.yaml')
