//! Pin the read-only open fallback that makes the CLI usable beside a running server.
//!
//! `agent0 serve` takes the Tantivy index writer for its lifetime and a crawl takes it for the
//! whole of a run, so any process that insists on its own writer cannot come up beside one.
//! `agent0 search` used to do exactly that and died with
//! `Failed to acquire Lockfile: LockBusy` while the server was up. `open_honouring_read_only`
//! is the fix, and these tests hold the writer open while a second open runs — the real
//! condition, hermetically, on a temp data root.

use std::path::Path;
use std::sync::Mutex;

use search_agent0::{open_honouring_read_only, Pipeline};
use search_core::config::Config;

fn test_config(data_dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.data_dir = data_dir.to_path_buf();
    cfg.user_agent = "agent0-read-only-open-test/0.1 (+test)".to_string();
    cfg.bind = "127.0.0.1:0".to_string();
    cfg
}

/// `SEARCH_READ_ONLY` is process-global; serialise the tests that set it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// With a writer already holding the index, the default (`auto`) open must fall back to
/// read-only rather than failing — and the plain writer open must still fail, which is what
/// makes the fallback load-bearing.
#[test]
fn auto_falls_back_to_read_only_when_the_writer_is_held() {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var("SEARCH_READ_ONLY").ok();
    std::env::remove_var("SEARCH_READ_ONLY");

    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    // The "server"/"crawl": open a writer and keep it alive for the whole test.
    let holder = Pipeline::open_with(&cfg, true).expect("writer open");

    // The bug: a plain writer open cannot coexist with the holder.
    assert!(
        Pipeline::open(&cfg).is_err(),
        "a second writer open must fail while the first holds the index"
    );

    // The fix: the honouring open degrades to read-only and still answers.
    let reader = open_honouring_read_only(&cfg).expect("read-only fallback open");
    assert!(
        reader.is_read_only(),
        "the fallback must open read-only, not silently retry as a writer"
    );

    drop(reader);
    drop(holder);
    if let Some(v) = saved {
        std::env::set_var("SEARCH_READ_ONLY", v);
    }
}

/// `SEARCH_READ_ONLY=0` means "I insist on the writer": the open must fail loudly rather than
/// quietly serve a read-only pipeline the caller did not ask for.
#[test]
fn explicit_writer_mode_fails_instead_of_falling_back() {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var("SEARCH_READ_ONLY").ok();

    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let holder = Pipeline::open_with(&cfg, true).expect("writer open");

    std::env::set_var("SEARCH_READ_ONLY", "0");
    let res = open_honouring_read_only(&cfg);

    match saved {
        Some(v) => std::env::set_var("SEARCH_READ_ONLY", v),
        None => std::env::remove_var("SEARCH_READ_ONLY"),
    }

    assert!(
        res.is_err(),
        "SEARCH_READ_ONLY=0 must fail on a locked index, not fall back"
    );
    drop(holder);
}

/// `SEARCH_READ_ONLY=1` never touches the writer, so it succeeds whether or not one is held.
#[test]
fn read_only_mode_always_opens() {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var("SEARCH_READ_ONLY").ok();
    std::env::set_var("SEARCH_READ_ONLY", "1");

    let dir = tempfile::TempDir::new().unwrap();
    let cfg = test_config(dir.path());
    let holder = Pipeline::open_with(&cfg, true).expect("writer open");

    let res = open_honouring_read_only(&cfg);

    match saved {
        Some(v) => std::env::set_var("SEARCH_READ_ONLY", v),
        None => std::env::remove_var("SEARCH_READ_ONLY"),
    }

    let reader = res.expect("SEARCH_READ_ONLY=1 must open beside a writer");
    assert!(reader.is_read_only());
    drop(reader);
    drop(holder);
}
