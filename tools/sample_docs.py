import sqlite3

con = sqlite3.connect('/var/lib/search/meta/meta.sqlite')
total_docs = con.execute('SELECT COUNT(*) FROM document_version').fetchone()[0]
decomposed_docs = con.execute("SELECT COUNT(DISTINCT resource_id) FROM doc_concept WHERE model_version = 'v2-decompose'").fetchone()[0]

print(f"Total documents: {total_docs}")
print(f"Decomposed documents: {decomposed_docs}")

# Check for terms related to romance, chemistry, psychology, neuroscience, love, pair bonding, oxytocin, etc.
keywords = ['romance', 'love', 'chemical', 'reaction', 'oxytocin', 'dopamine', 'neuro', 'brain', 'biology', 'psychology', 'pair bonding', 'attraction', 'hormone']
print("\nSearching corpus for target query keywords:")
for kw in keywords:
    rows = con.execute("SELECT resource_id, title FROM document_version WHERE lower(title) LIKE ? LIMIT 3", (f"%{kw}%",)).fetchall()
    print(f"  Keyword '{kw}': {len(rows)} sample matches")
    for r in rows:
        print(f"    - {r[0]}: {r[1]}")
