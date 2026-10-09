//! On-disk companion to the graph rows: entity → documents, plus the edge list.
//!
//! The meta store keeps entities, aliases and edges, but **nothing joins an entity to the
//! documents that mention it** — `agent0 graph build` computes that mapping to decide which
//! entities recur, then drops it. Without the join a query-time activation can name related
//! entities but cannot turn them into results, which is why §9.4 needs this file.
//!
//! It is a sidecar rather than extra columns because the index schema is frozen and
//! hash-asserted: adding tables would invalidate every existing index. The dense lane took the
//! same route for its vectors.
//!
//! Format (little-endian, all lengths `u32`, validated against the file length before any
//! allocation so a truncated or hostile file is an `io::Error`, never a panic or a huge alloc):
//!
//! ```text
//! magic  "SGRAPH01"                                  8 bytes
//! u32    entity count
//!   per entity: u32 id len + bytes
//!               u32 name len + bytes
//!               u32 alias count, then that many (u32 len + bytes)
//!               u32 doc count, then that many (u32 len + bytes)
//! u32    edge count
//!   per edge: u32 from len + bytes, u32 to len + bytes, f32 weight
//! ```

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

use crate::{Edge, EdgeKind, Entity, EntityKind, Graph};

/// Magic bytes; the trailing digits are the format version.
pub const MAGIC: &[u8; 8] = b"SGRAPH01";

/// A refusal rather than an allocation: no real corpus needs ten million entities, so a larger
/// count means the file is corrupt (or not ours) and parsing stops.
const MAX_ROWS: u32 = 10_000_000;

/// One entity with the documents it was found in.
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarEntity {
    pub entity_id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub docs: Vec<String>,
}

/// One co-occurrence edge.
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarEdge {
    pub from: String,
    pub to: String,
    pub weight: f32,
}

/// Everything the query-time lane needs, in load order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphSidecar {
    pub entities: Vec<SidecarEntity>,
    pub edges: Vec<SidecarEdge>,
}

impl GraphSidecar {
    /// Rebuild a traversal graph: entities keyed by id (name and aliases indexed for lookup), each
    /// edge weighted as recorded.
    ///
    /// `RelatedTo` is used for the co-occurrence edges the build produces, so activation spreads
    /// along exactly the relationships the extractor measured.
    pub fn to_graph(&self) -> Graph {
        let mut graph = Graph::new();
        for entity in &self.entities {
            let mut node = Entity::new(
                entity.entity_id.clone(),
                entity.name.clone(),
                EntityKind::Concept,
            );
            node.aliases = entity.aliases.clone();
            graph.upsert_entity(node);
        }
        for edge in &self.edges {
            graph.add_edge(Edge::new(
                edge.from.clone(),
                edge.to.clone(),
                EdgeKind::RelatedTo,
                edge.weight,
            ));
        }
        graph
    }

    /// Documents mentioning `entity_id`, or an empty slice when the id is unknown.
    pub fn docs_for(&self, entity_id: &str) -> &[String] {
        self.entities
            .iter()
            .find(|e| e.entity_id == entity_id)
            .map(|e| e.docs.as_slice())
            .unwrap_or(&[])
    }

    /// Entity id → documents, built once for a lane that looks up many entities per query.
    pub fn docs_index(&self) -> HashMap<&str, &[String]> {
        self.entities.iter().map(|e| (e.entity_id.as_str(), e.docs.as_slice())).collect()
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

/// Serialise to `path`, creating parent directories.
pub fn write(path: &Path, sidecar: &GraphSidecar) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(MAGIC);
    let put_u32 = |out: &mut Vec<u8>, v: usize| out.extend_from_slice(&(v as u32).to_le_bytes());
    let put_str = |out: &mut Vec<u8>, s: &str| {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    };

    put_u32(&mut out, sidecar.entities.len());
    for entity in &sidecar.entities {
        put_str(&mut out, &entity.entity_id);
        put_str(&mut out, &entity.name);
        put_u32(&mut out, entity.aliases.len());
        for alias in &entity.aliases {
            put_str(&mut out, alias);
        }
        put_u32(&mut out, entity.docs.len());
        for doc in &entity.docs {
            put_str(&mut out, doc);
        }
    }
    put_u32(&mut out, sidecar.edges.len());
    for edge in &sidecar.edges {
        put_str(&mut out, &edge.from);
        put_str(&mut out, &edge.to);
        out.extend_from_slice(&edge.weight.to_le_bytes());
    }

    // Write to a temporary file and rename, so a reader never sees a half-written sidecar.
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&out)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Read a sidecar. Any structural problem — short file, bad magic, a length that overruns the
/// buffer — is an `io::Error` describing it.
pub fn load(path: &Path) -> io::Result<GraphSidecar> {
    let bytes = std::fs::read(path)?;
    parse(&bytes)
}

/// Parse an in-memory image; exposed for tests and for callers that already hold the bytes.
pub fn parse(bytes: &[u8]) -> io::Result<GraphSidecar> {
    let mut cursor = Cursor { bytes, at: 0 };
    if bytes.len() < MAGIC.len() || cursor.take(MAGIC.len())? != MAGIC {
        return Err(invalid("not a graph sidecar (bad magic)"));
    }
    let entity_count = cursor.u32()?;
    if entity_count > MAX_ROWS {
        return Err(invalid("implausible entity count"));
    }
    let mut entities = Vec::with_capacity(entity_count.min(4096) as usize);
    for _ in 0..entity_count {
        let entity_id = cursor.string()?;
        let name = cursor.string()?;
        let alias_count = cursor.u32()?;
        if alias_count > MAX_ROWS {
            return Err(invalid("implausible alias count"));
        }
        let mut aliases = Vec::new();
        for _ in 0..alias_count {
            aliases.push(cursor.string()?);
        }
        let doc_count = cursor.u32()?;
        if doc_count > MAX_ROWS {
            return Err(invalid("implausible document count"));
        }
        let mut docs = Vec::new();
        for _ in 0..doc_count {
            docs.push(cursor.string()?);
        }
        entities.push(SidecarEntity { entity_id, name, aliases, docs });
    }
    let edge_count = cursor.u32()?;
    if edge_count > MAX_ROWS {
        return Err(invalid("implausible edge count"));
    }
    let mut edges = Vec::with_capacity(edge_count.min(4096) as usize);
    for _ in 0..edge_count {
        let from = cursor.string()?;
        let to = cursor.string()?;
        let weight = f32::from_le_bytes(cursor.array4()?);
        edges.push(SidecarEdge { from, to, weight });
    }
    Ok(GraphSidecar { entities, edges })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("graph sidecar: {message}"))
}

/// A bounds-checked reader: every accessor proves it stays inside the buffer.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(|| invalid("length overflow"))?;
        if end > self.bytes.len() {
            return Err(invalid("file ended early"));
        }
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.array4()?))
    }

    fn array4(&mut self) -> io::Result<[u8; 4]> {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(self.take(4)?);
        Ok(buf)
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        // The bound check happens before any allocation: `take` refuses a length that overruns.
        let raw = self.take(len)?;
        String::from_utf8(raw.to_vec()).map_err(|_| invalid("string is not valid UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GraphSidecar {
        GraphSidecar {
            entities: vec![
                SidecarEntity {
                    entity_id: "vllm".to_string(),
                    name: "vLLM".to_string(),
                    aliases: vec!["VLLM".to_string()],
                    docs: vec!["doc-a".to_string(), "doc-b".to_string()],
                },
                SidecarEntity {
                    entity_id: "paged attention".to_string(),
                    name: "Paged Attention".to_string(),
                    aliases: vec![],
                    docs: vec!["doc-b".to_string()],
                },
            ],
            edges: vec![SidecarEdge {
                from: "vllm".to_string(),
                to: "paged attention".to_string(),
                weight: 3.0,
            }],
        }
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("graph-sidecar-{}", std::process::id()));
        let path = dir.join("graph.bin");
        write(&path, &sample()).expect("write");
        let back = load(&path).expect("load");
        assert_eq!(back, sample());
        assert_eq!(back.docs_for("vllm"), ["doc-a".to_string(), "doc-b".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_entity_has_no_documents_rather_than_panicking() {
        assert!(sample().docs_for("never-mentioned").is_empty());
    }

    #[test]
    fn the_graph_rebuilt_from_the_file_uses_edges_and_resolves_aliases() {
        let graph = sample().to_graph();
        assert_eq!(graph.entity_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        // The alias is indexed, so a query naming "VLLM" resolves to the canonical entity.
        let resolved = graph.resolve_name("VLLM");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].entity_id, "vllm");
    }

    #[test]
    fn docs_index_covers_every_entity() {
        let sidecar = sample();
        let index = sidecar.docs_index();
        assert_eq!(index.len(), 2);
        assert_eq!(index.get("paged attention").map(|d| d.len()), Some(1));
    }

    #[test]
    fn a_truncated_file_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("graph-sidecar-trunc-{}", std::process::id()));
        let path = dir.join("graph.bin");
        write(&path, &sample()).expect("write");
        let full = std::fs::read(&path).expect("read");
        // Every prefix of a valid file must fail cleanly; a panic here is the bug this guards.
        for cut in [0usize, 3, 8, 12, full.len() - 1] {
            let err = parse(&full[..cut]).expect_err("a short buffer must not parse");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "cut at {cut}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_magic_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"NOTGRAPH");
        let err = parse(&bytes).expect_err("bad magic");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_length_that_overruns_the_buffer_is_refused_without_allocating() {
        // Valid magic, an entity count of 1, then a string length of 4 GiB.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = parse(&bytes).expect_err("an overrunning length must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_empty_graph_round_trips() {
        let dir = std::env::temp_dir().join(format!("graph-sidecar-empty-{}", std::process::id()));
        let path = dir.join("graph.bin");
        write(&path, &GraphSidecar::default()).expect("write");
        let back = load(&path).expect("load");
        assert_eq!(back.entity_count(), 0);
        assert_eq!(back.edge_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
