import sqlite3

con = sqlite3.connect('/var/lib/search/meta/meta.sqlite')
for table in ['resource', 'content', 'document_version']:
    cols = con.execute(f"PRAGMA table_info({table})").fetchall()
    print(f"\nTable {table}:")
    for col in cols:
        print(f"  {col[1]} ({col[2]})")
