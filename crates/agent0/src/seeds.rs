//! `agent0 seeds` — generate crawler seed files.
//!
//! ```text
//! agent0 seeds arxiv [--query Q] [--max N] [--out FILE]
//! agent0 seeds oai   [--repo SPEC]... [--from D] [--until D] [--out FILE]
//! ```
//!
//! `arxiv` queries the arXiv Atom API through the injectable
//! [`search_sources::arxiv::HttpGet`] transport and writes the canonical paper URLs, one per line.
//! `oai` writes OAI-PMH `ListRecords` URLs for a configurable repository set (`--repo
//! name=base_url[#set]`, comma/semicolon separated; the default set is arXiv's OAI endpoint). The
//! output feeds `agent0 crawl <seedsfile>`.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use search_sources::arxiv::{self, FetchError, HttpGet};

/// Default `--out` for both subcommands.
const DEFAULT_OUT: &str = "seeds.txt";

/// Real HTTP transport over the arXiv/OAI endpoints: the production-side implementation of
/// [`HttpGet`]. The wrapper itself lives in `search-sources`; only the network lives here.
struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    fn new() -> Result<Self, FetchError> {
        let user_agent = std::env::var("SEARCH_USER_AGENT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "agent0-seeds/0.1 (arxiv feed fetch)".to_string());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(user_agent)
            .build()
            .map_err(|e| FetchError::Http(e.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl HttpGet for ReqwestTransport {
    async fn get(&self, url: &str) -> Result<String, FetchError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| FetchError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FetchError::Http(format!("HTTP {status} for {url}")));
        }
        resp.text().await.map_err(|e| FetchError::Http(e.to_string()))
    }
}

/// Dispatch `agent0 seeds <arxiv|oai> ...`.
pub async fn run(args: &[String]) -> ExitCode {
    match args.get(2).map(String::as_str) {
        Some("arxiv") => run_arxiv(args).await,
        Some("oai") => run_oai(args),
        _ => {
            eprintln!(
                "usage: agent0 seeds arxiv [--query Q] [--max N] [--out FILE]\n       \
                        agent0 seeds oai [--repo NAME=URL[#SET]]... [--from DATE] [--until DATE] [--out FILE]"
            );
            ExitCode::from(2)
        }
    }
}

/// `agent0 seeds arxiv [--query Q] [--max N] [--out FILE]`
async fn run_arxiv(args: &[String]) -> ExitCode {
    let query = flag_str(args, "--query").unwrap_or_else(|| "all:*".to_string());
    let max = flag_usize(args, "--max").unwrap_or(100);
    let out = flag_str(args, "--out").unwrap_or_else(|| DEFAULT_OUT.to_string());

    let transport = match ReqwestTransport::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("seeds: could not build the HTTP transport: {e}");
            return ExitCode::from(1);
        }
    };
    let links = match collect_arxiv_seeds(&transport, &query, max).await {
        Ok(links) => links,
        Err(e) => {
            eprintln!("seeds: arxiv fetch failed: {e}");
            return ExitCode::from(1);
        }
    };
    write_seed_file(Path::new(&out), &links)
}

/// `agent0 seeds oai [--repo SPEC]... [--from DATE] [--until DATE] [--out FILE]`
fn run_oai(args: &[String]) -> ExitCode {
    let specs = flag_all(args, "--repo");
    let repos = if specs.is_empty() {
        arxiv::default_oai_repositories()
    } else {
        specs
            .iter()
            .flat_map(|s| arxiv::parse_oai_repositories(s))
            .collect::<Vec<_>>()
    };
    if repos.is_empty() {
        eprintln!("seeds: no usable OAI repositories (expected name=base_url[#set])");
        return ExitCode::from(2);
    }
    let from = flag_str(args, "--from");
    let until = flag_str(args, "--until");
    let urls = arxiv::oai_list_records_urls(&repos, from.as_deref(), until.as_deref());
    let out = flag_str(args, "--out").unwrap_or_else(|| DEFAULT_OUT.to_string());
    write_seed_file(Path::new(&out), &urls)
}

/// Fetch an arXiv feed through `client` and return the canonical links. Generic over the
/// transport so tests can drive it with a canned body and no network.
pub async fn collect_arxiv_seeds<C: HttpGet + ?Sized>(
    client: &C,
    query: &str,
    max_results: usize,
) -> Result<Vec<String>, FetchError> {
    arxiv::fetch_arxiv_links(client, query, max_results).await
}

/// One seed URL per line, each terminated by a newline (so a heredoc-ish file has no missing
/// final line). An empty list renders an empty string.
pub fn render_seeds(entries: &[String]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str(entry.trim());
        out.push('\n');
    }
    out
}

/// Resolve `--out` to a path, creating no directories: a missing parent is a user error.
fn write_seed_file(path: &Path, entries: &[String]) -> ExitCode {
    match std::fs::write(path, render_seeds(entries)) {
        Ok(()) => {
            println!("wrote {} seed URL(s) to {}", entries.len(), path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("seeds: failed to write {}: {e}", path.display());
            ExitCode::from(1)
        }
    }
}

/// `--flag VALUE`, when the value is present and non-empty.
fn flag_str(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.trim().is_empty())
        .cloned()
}

/// `--flag N`, when the value parses as a `usize`.
fn flag_usize(args: &[String], flag: &str) -> Option<usize> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
}

/// Every non-empty value following an occurrence of `flag`.
fn flag_all(args: &[String], flag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            if let Some(v) = args.get(i + 1).filter(|v| !v.trim().is_empty()) {
                out.push(v.clone());
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_sources::arxiv::OaiRepository;

    const FEED: &str = r#"<feed xmlns="http://www.w3.org/2005/Atom">
      <entry><id>http://arxiv.org/abs/2401.11111v2</id></entry>
      <entry><id>http://arxiv.org/abs/2402.22222v1</id></entry>
    </feed>"#;

    struct Stub {
        body: Result<String, FetchError>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl HttpGet for Stub {
        async fn get(&self, url: &str) -> Result<String, FetchError> {
            self.seen.lock().unwrap().push(url.to_string());
            self.body.clone()
        }
    }

    #[tokio::test]
    async fn collect_and_render_seeds_from_stub_transport() {
        let stub = Stub {
            body: Ok(FEED.to_string()),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let links = collect_arxiv_seeds(&stub, "all:electron", 2).await.unwrap();
        assert_eq!(
            links,
            vec![
                "https://arxiv.org/abs/2401.11111".to_string(),
                "https://arxiv.org/abs/2402.22222".to_string(),
            ]
        );
        assert_eq!(
            render_seeds(&links),
            "https://arxiv.org/abs/2401.11111\nhttps://arxiv.org/abs/2402.22222\n"
        );

        let seen = stub.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("search_query=all%3Aelectron"));
        assert!(seen[0].contains("max_results=2"));
    }

    #[tokio::test]
    async fn collect_propagates_transport_failure() {
        let stub = Stub {
            body: Err(FetchError::Http("timed out".to_string())),
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let err = collect_arxiv_seeds(&stub, "all:x", 1).await.unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn render_seeds_is_empty_for_no_entries() {
        assert_eq!(render_seeds(&[]), "");
    }

    #[test]
    fn oai_seeds_default_to_the_arxiv_endpoint() {
        let repos = arxiv::default_oai_repositories();
        let urls = arxiv::oai_list_records_urls(&repos, None, None);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("export.arxiv.org/oai2"));
        assert!(urls[0].contains("verb=ListRecords"));
    }

    #[test]
    fn flag_all_collects_every_occurrence() {
        let args: Vec<String> = ["seeds", "oai", "--repo", "a=http://a", "--repo", "b=http://b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag_all(&args, "--repo"), vec!["a=http://a", "b=http://b"]);
        let repos: Vec<OaiRepository> = flag_all(&args, "--repo")
            .iter()
            .flat_map(|s| arxiv::parse_oai_repositories(s))
            .collect();
        assert_eq!(repos.len(), 2);
    }
}
