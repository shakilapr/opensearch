import sqlite3
from urllib.parse import urlparse

con = sqlite3.connect('/var/lib/search/meta/meta.sqlite')
rows = con.execute("SELECT url FROM resource LIMIT 5000").fetchall()
domain_counts = {}
for (u,) in rows:
    try:
        netloc = urlparse(u).netloc
        domain_counts[netloc] = domain_counts.get(netloc, 0) + 1
    except Exception:
        pass

sorted_domains = sorted(domain_counts.items(), key=lambda x: x[1], reverse=True)
print("Top crawled domains in sample:")
for domain, count in sorted_domains[:20]:
    print(f"  {domain}: {count}")
