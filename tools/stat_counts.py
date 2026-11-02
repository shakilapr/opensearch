import sqlite3

con = sqlite3.connect('/var/lib/search/meta/meta.sqlite')
concepts = con.execute("SELECT COUNT(concept_id) FROM concept").fetchone()[0]
aliases = con.execute("SELECT COUNT(alias) FROM concept_alias").fetchone()[0]
doc_concepts = con.execute("SELECT COUNT(concept_id) FROM doc_concept").fetchone()[0]
v2_assocs = con.execute("SELECT COUNT(concept_id) FROM doc_concept WHERE model_version = 'v2-decompose'").fetchone()[0]

print(f"Total Canonical Concepts: {concepts}")
print(f"Total Concept Aliases:    {aliases}")
print(f"Total Doc-Concept Links:  {doc_concepts}")
print(f"  V2 Dense Associations:  {v2_assocs}")
