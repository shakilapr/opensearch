//! PolicyTrace (plan Stage 8): structured per-query phase events for evaluation + distillation.
//! Interfaces frozen in `docs/specs/STAGE8_SPEC.md`.
//!
//! A [`PolicyTrace`] wraps a [`TraceSink`] and emits one [`TraceEvent`] per pipeline phase. The
//! [`JsonlTraceSink`] appends events as newline-delimited JSON; [`read_trace`] reads them back
//! losslessly (serde round-trip on the whole [`TraceEvent`]).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Pipeline phases a query passes through. Ordering mirrors the retrieval loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Query,
    Analyze,
    Route,
    Retrieve,
    Refresh,
    Rerank,
    Navigate,
    Answer,
}

/// One structured observation from the pipeline. `detail` and `policy_tier` are optional
/// extensions carried verbatim through the JSONL round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub ts_ms: u64,
    pub query_id: String,
    pub phase: Phase,
    pub action: String,
    pub detail: BTreeMap<String, String>,
    pub policy_tier: Option<String>,
    pub cost_ms: u64,
}

/// Destination for [`TraceEvent`]s. `&self` so a sink can be shared; implementations must be
/// usable from multiple threads.
pub trait TraceSink: Send + Sync {
    fn emit(&self, ev: &TraceEvent);
}

/// Sink that discards everything (the default no-op path).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl TraceSink for NullSink {
    fn emit(&self, _ev: &TraceEvent) {}
}

/// Appends events as JSONL to a file. Each `emit` writes one line and flushes so a concurrent
/// [`read_trace`] observes every event written so far.
pub struct JsonlTraceSink {
    writer: Mutex<BufWriter<File>>,
}

impl JsonlTraceSink {
    /// Open (creating if needed, appending if present) the JSONL file at `path`. Missing parent
    /// directories are created.
    pub fn new(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }
}

impl TraceSink for JsonlTraceSink {
    fn emit(&self, ev: &TraceEvent) {
        let line = match serde_json::to_string(ev) {
            Ok(line) => line,
            Err(_) => return,
        };
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(writer, "{line}");
        let _ = writer.flush();
    }
}

/// Monotonic counter backing [`PolicyTrace::new_query_id`].
static QUERY_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-query front door over a [`TraceSink`]: owns the current `query_id` and stamps events with
/// a wall-clock `ts_ms`.
pub struct PolicyTrace {
    sink: Box<dyn TraceSink>,
    query_id: Mutex<String>,
}

impl PolicyTrace {
    /// Wrap a sink with an empty current query id (set one with [`new_query_id`](Self::new_query_id)).
    pub fn new(sink: Box<dyn TraceSink>) -> Self {
        Self {
            sink,
            query_id: Mutex::new(String::new()),
        }
    }

    /// Mint a fresh, process-unique query id and make it the current one. Subsequent
    /// [`emit`](Self::emit) calls are stamped with it.
    pub fn new_query_id(&self) -> String {
        let n = QUERY_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("q-{n:016x}");
        *self.query_id.lock().unwrap_or_else(|e| e.into_inner()) = id.clone();
        id
    }

    /// Emit one phase event under the current query id. `detail` is empty and `policy_tier` is
    /// `None`; use [`JsonlTraceSink`]/[`TraceEvent`] directly for richer events.
    pub fn emit(&self, phase: Phase, action: impl Into<String>, cost_ms: u64) {
        let query_id = self
            .query_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let ev = TraceEvent {
            ts_ms: now_ms(),
            query_id,
            phase,
            action: action.into(),
            detail: BTreeMap::new(),
            policy_tier: None,
            cost_ms,
        };
        self.sink.emit(&ev);
    }
}

/// Read a JSONL trace file back into events. Blank lines are skipped; a malformed line is an
/// `InvalidData` I/O error.
pub fn read_trace(path: &Path) -> std::io::Result<Vec<TraceEvent>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ev = serde_json::from_str(trimmed)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        events.push(ev);
    }
    Ok(events)
}

/// Milliseconds since the Unix epoch (0 if the clock is before it).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Sink that records everything; clone the handle to inspect what was emitted.
    #[derive(Clone, Default)]
    struct RecordingSink {
        events: Arc<Mutex<Vec<TraceEvent>>>,
    }

    impl RecordingSink {
        fn events(&self) -> Vec<TraceEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl TraceSink for RecordingSink {
        fn emit(&self, ev: &TraceEvent) {
            self.events.lock().unwrap().push(ev.clone());
        }
    }

    fn sample(ts_ms: u64, query_id: &str, phase: Phase, action: &str) -> TraceEvent {
        TraceEvent {
            ts_ms,
            query_id: query_id.to_string(),
            phase,
            action: action.to_string(),
            detail: BTreeMap::new(),
            policy_tier: None,
            cost_ms: 0,
        }
    }

    #[test]
    fn jsonl_round_trip_is_lossless() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("trace.jsonl");
        let sink = JsonlTraceSink::new(&path).unwrap();

        let ev1 = TraceEvent {
            ts_ms: 1_234,
            query_id: "q-1".into(),
            phase: Phase::Retrieve,
            action: "fetch".into(),
            detail: BTreeMap::from([
                ("url".to_string(), "https://example.com/a".to_string()),
                ("engine".to_string(), "bm25".to_string()),
            ]),
            policy_tier: Some("fast".into()),
            cost_ms: 42,
        };
        let ev2 = TraceEvent {
            ts_ms: 9_999,
            query_id: "q-2".into(),
            phase: Phase::Answer,
            action: "emit".into(),
            detail: BTreeMap::new(),
            policy_tier: None,
            cost_ms: 0,
        };

        sink.emit(&ev1);
        sink.emit(&ev2);

        let back = read_trace(&path).unwrap();
        assert_eq!(back, vec![ev1, ev2]);

        // Every Phase survives the round trip.
        for phase in [
            Phase::Query,
            Phase::Analyze,
            Phase::Route,
            Phase::Retrieve,
            Phase::Refresh,
            Phase::Rerank,
            Phase::Navigate,
            Phase::Answer,
        ] {
            let ev = sample(0, "q", phase, "a");
            let json = serde_json::to_string(&ev).unwrap();
            assert_eq!(serde_json::from_str::<TraceEvent>(&json).unwrap(), ev);
        }
    }

    #[test]
    fn policy_trace_emits_phase_action_and_query_id() {
        let sink = RecordingSink::default();
        let handle = sink.clone();
        let trace = PolicyTrace::new(Box::new(sink));

        let qid = trace.new_query_id();
        assert!(!qid.is_empty());

        trace.emit(Phase::Query, "start", 0);
        trace.emit(Phase::Route, "pick_engine", 5);

        let events = handle.events();
        assert_eq!(events.len(), 2);

        assert_eq!(events[0].phase, Phase::Query);
        assert_eq!(events[0].action, "start");
        assert_eq!(events[0].query_id, qid);
        assert_eq!(events[0].cost_ms, 0);
        assert!(events[0].detail.is_empty());
        assert!(events[0].policy_tier.is_none());

        assert_eq!(events[1].phase, Phase::Route);
        assert_eq!(events[1].action, "pick_engine");
        assert_eq!(events[1].query_id, qid);
        assert_eq!(events[1].cost_ms, 5);

        // A fresh query id retags subsequent events.
        let qid2 = trace.new_query_id();
        assert_ne!(qid, qid2);
        trace.emit(Phase::Answer, "done", 1);
        assert_eq!(handle.events()[2].query_id, qid2);
    }

    #[test]
    fn policy_trace_round_trips_through_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let trace = PolicyTrace::new(Box::new(JsonlTraceSink::new(&path).unwrap()));
        let qid = trace.new_query_id();
        trace.emit(Phase::Analyze, "parse", 3);
        trace.emit(Phase::Answer, "final", 7);

        let events = read_trace(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.query_id == qid));
        assert_eq!(events[0].phase, Phase::Analyze);
        assert_eq!(events[1].action, "final");
    }

    #[test]
    fn null_sink_and_empty_read_are_noops() {
        let trace = PolicyTrace::new(Box::new(NullSink));
        trace.new_query_id();
        trace.emit(Phase::Query, "x", 0); // must not panic

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        JsonlTraceSink::new(&path).unwrap();
        assert!(read_trace(&path).unwrap().is_empty());
    }
}
