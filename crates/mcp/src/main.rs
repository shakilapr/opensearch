//! `search-mcp` binary: exposes the local search-api over the Model Context
//! Protocol (JSON-RPC 2.0 over stdio).
//!
//! The protocol layer lives in the library; this binary supplies the concrete
//! [`Backend`] — a thin loopback client of the HTTP search-api
//! ([`search_mcp::HttpBackend`]) — and runs the stdio loop. The API base is
//! `SEARCH_API_BASE`, falling back to `http://<SEARCH_BIND>` and finally
//! `http://127.0.0.1:8080`.
//!
//! ```text
//! search-mcp          # reads newline-delimited JSON-RPC on stdin
//! ```

#![forbid(unsafe_code)]

use std::process::ExitCode;

use search_mcp::{Backend, HttpBackend};

/// The default search-api base when no environment override is set.
const DEFAULT_API_BASE: &str = "http://127.0.0.1:8080";

fn api_base() -> String {
    if let Ok(base) = std::env::var("SEARCH_API_BASE") {
        if !base.trim().is_empty() {
            return base;
        }
    }
    if let Ok(bind) = std::env::var("SEARCH_BIND") {
        if !bind.trim().is_empty() {
            return format!("http://{bind}");
        }
    }
    DEFAULT_API_BASE.to_string()
}

fn main() -> ExitCode {
    let backend = match HttpBackend::new(api_base()) {
        Ok(b) => Box::new(b) as Box<dyn Backend>,
        Err(e) => {
            eprintln!("search-mcp: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = search_mcp::serve(backend) {
        eprintln!("search-mcp: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
