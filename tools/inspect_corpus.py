import sqlite3

con = sqlite3.connect('/var/lib/search/meta/meta.sqlite')
# Let's inspect Wikipedia articles and science articles in the corpus
rows = con.execute("SELECT resource_id, title FROM document_version WHERE title LIKE '%Wikipedia%' LIMIT 30").fetchall()
print(f"Total Wikipedia articles sampled: {len(rows)}")
for r in rows:
    print(f"  {r[0]}: {r[1]}")

print("\nSampling Nature/Science/Scholar articles:")
rows2 = con.execute("SELECT resource_id, title FROM document_version WHERE title LIKE '%Nature%' OR title LIKE '%Scholar%' OR title LIKE '%Science%' LIMIT 20").fetchall()
for r in rows2:
    print(f"  {r[0]}: {r[1]}")
