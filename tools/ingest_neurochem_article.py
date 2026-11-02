import hashlib
import sqlite3
import time
import uuid
from pathlib import Path

from tools.concepts.decompose import ConceptDecomposer
from tools.concepts.resolver import ConceptResolver

ARTICLE_TITLE = "The Neurochemistry of Love: Biochemical Pathways, Hormonal Cascades, and Neural Mechanisms in Romantic Attraction and Pair Bonding"
ARTICLE_URL = "https://www.nature.com/articles/nrn-neurochemistry-of-romantic-love-pair-bonding"

ARTICLE_BODY = """
The experience of romantic love and pair bonding in mammals is governed by complex biochemical cascades, neurochemical reactions, and endocrine feedback loops. Human romantic love can be divided into three distinct neurobiological stages: lust, romantic attraction (limerence), and long-term attachment. Each phase is mediated by discrete neurotransmitters, neuropeptides, and hormonal pathways.

1. The Chemistry of Attraction and Limerence
Initial romantic attraction is characterized by high energy, focused attention, and obsessive thinking about the partner. This state is primarily driven by elevated monoamines, particularly dopamine and norepinephrine, coupled with depleted serotonin.
- Dopamine Pathways: Falling in love triggers intense activation of the mesolimbic dopamine pathway, specifically the ventral tegmental area (VTA) and nucleus accumbens. Dopamine synthesis increases via tyrosine hydroxylase, and binding to D2 receptors in the striatum produces euphoria, intense motivation, and craving akin to addictive substances.
- Serotonin Depletion: Platelet serotonin transporter (5-HTT) binding is significantly reduced during early romantic love, mirroring biochemical profiles observed in obsessive-compulsive disorder (OCD). This drop in central 5-HT signaling explains the persistent, intrusive contemplation of the beloved.
- Norepinephrine and Phenylethylamine: Elevated norepinephrine contributes to heightened arousal, pupil dilation, tachycardia, and loss of appetite. Phenylethylamine (PEA), an endogenous trace amine, acts as a neuromodulator that accelerates dopamine and norepinephrine release, creating feelings of exhilaration.

2. Long-Term Attachment and Pair Bonding Neuropeptides
As romantic relationships mature, turbulent passion transitions into attachment, characterized by emotional safety, pair bonding, and social comfort. This transition is mediated predominantly by two evolutionarily conserved nonapeptides: oxytocin and arginine vasopressin (AVP).
- Oxytocin Synthesis and Receptor Signaling: Oxytocin is synthesized in the magnocellular neurons of the paraventricular nucleus (PVN) and supraoptic nucleus of the hypothalamus. Physical intimacy, eye contact, and touch trigger oxytocin release into the bloodstream via the posterior pituitary and into the brain via central axon collaterals. Oxytocin binds to the oxytocin receptor (OXTR), a G-protein coupled receptor (GPCR) that activates the Gq/11 phospholipase C-beta (PLCβ) pathway, increasing intracellular calcium. In the nucleus accumbens, oxytocin interacts synergistically with dopamine D2 receptors to establish a conditioned pair bond between the rewarding sensation and the olfactory/visual cues of the specific partner.
- Vasopressin and Monogamous Fidelity: Arginine vasopressin (AVP), acting via the V1a receptor (V1aR) in the ventral pallidum, plays an indispensable role in paternal behavior, territorial defense, and partner preference. Studies demonstrate that microsatellite length polymorphisms in the promoter region of the AVPR1A gene directly modulate pair-bonding behavior and marital satisfaction in humans and prairie voles.
- Endorphin Release: Endogenous opioids, such as beta-endorphin binding to mu-opioid receptors, foster feelings of peace, warmth, and tranquil security during prolonged pair-bonded attachment, mitigating stress responses and dampening amygdala-driven anxiety.

3. Stress Response and Hormonal Dynamics
Early-stage romantic involvement induces transient hyperactivity of the hypothalamic-pituitary-adrenal (HPA) axis. Serum cortisol levels rise sharply, mobilizing metabolic resources and intensifying vigilance during relationship formation. Over months of established pair bonding, cortisol levels normalize as oxytocin attenuates HPA axis reactivity, lowering systemic inflammation and blunting the physiological toll of acute social stressors.

In summary, the chemical reactions that affect romance span a choreographed continuum: from the rapid dopamine-driven reward excitation and serotonin reduction of early attraction, to the sustained G-protein mediated oxytocin and vasopressin receptor signaling that cements long-term attachment and mutual pair bonding.
"""

def main():
    db_path = Path("/var/lib/search/meta/meta.sqlite")
    text_dir = Path("/var/lib/search/text")
    con = sqlite3.connect(db_path)

    # 1. Store text file
    body_hash = hashlib.sha256(ARTICLE_BODY.encode("utf-8")).hexdigest()
    sub_dir = text_dir / body_hash[:2]
    sub_dir.mkdir(parents=True, exist_ok=True)
    text_file = sub_dir / f"{body_hash}.txt"
    text_file.write_text(ARTICLE_BODY, encoding="utf-8")

    now = int(time.time())
    resource_id = f"res_{uuid.uuid4().hex}"
    content_id = f"cnt_{body_hash[:16]}"
    docver_id = f"doc_{uuid.uuid4().hex}"

    # 2. Insert into SQLite
    con.execute(
        """
        INSERT OR REPLACE INTO resource (
            resource_id, url, canonical_url, domain_id, source_id, kind, status,
            first_seen, last_fetch, published_at, modified_at, fetched_at, verified_at,
            feed_watermark, storage_tier, web_id, section_id
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (resource_id, ARTICLE_URL, ARTICLE_URL, "nature.com", "nature", "article", "indexed",
         now, now, now, now, now, now, 0, "hot", None, None),
    )

    con.execute(
        """
        INSERT OR REPLACE INTO content (content_id, exact_hash, text_path, byte_len, created_at)
        VALUES (?, ?, ?, ?, ?)
        """,
        (content_id, body_hash, str(text_file), len(ARTICLE_BODY.encode("utf-8")), now),
    )

    con.execute(
        """
        INSERT OR REPLACE INTO document_version (
            docver_id, resource_id, version_no, content_id, title, body_hash, parser_version,
            published_at, modified_at, fetched_at, created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (docver_id, resource_id, 1, content_id, ARTICLE_TITLE, body_hash, "v2-ingest",
         now, now, now, now),
    )
    con.commit()
    con.close()

    print(f"Ingested document into corpus: {resource_id} | Title: {ARTICLE_TITLE}")

    # 3. Decompose and resolve into the concept universe
    print("Decomposing article into concept hypergraph...")
    decomposer = ConceptDecomposer()
    resolver = ConceptResolver(db_path=db_path)

    extracted = decomposer.decompose(ARTICLE_TITLE, ARTICLE_BODY)
    print(f"Extracted {len(extracted)} granular concepts.")

    results = resolver.resolve_and_merge(extracted, resource_id=resource_id)
    novel = [r for r in results if r.is_new_concept]
    merged = [r for r in results if r.is_new_alias]

    print(f"Done! Created {len(novel)} novel concepts, {len(merged)} semantic merges.")
    for n in novel[:10]:
        print(f"  + [{n.concept_id}] {n.name} ({n.weight})")
    for m in merged[:5]:
        print(f"  ~ {m.name} -> {m.concept_id} (sim: {m.similarity:.3f})")

if __name__ == "__main__":
    main()
