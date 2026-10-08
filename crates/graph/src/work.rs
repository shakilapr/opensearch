//! Scholarly work identity: DOI / arXiv / PMID normalization (plan Stage 1, crawler §24).
//!
//! A paper can live at many locations (publisher, preprint, repository, author copy); they all
//! resolve to one [`WorkId`](../../search_core/ids/struct.WorkId.html) canonical string. This
//! module owns the *string* normalization and free-text detection; persistence lives in
//! `search-metadata`.
//!
//! Canonical form: `"doi:10.1234/x"`, `"arxiv:2301.00001"`, `"pmid:12345678"`.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Which scholarly identifier space a value belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkIdKind {
    Doi,
    Arxiv,
    Pmid,
}

/// Normalize a bare identifier into its canonical WorkId string, or `None` if it is not a
/// well-formed identifier of `kind`.
///
/// * **DOI** — trimmed, scheme/label prefixes (`https://doi.org/`, `http(s)://dx.doi.org/`,
///   `doi:`) stripped, lowercased; must match `10.\d{4,}/…`.
/// * **arXiv** — `arXiv:` prefix stripped; old-style ids (`math.GT/0309136`) kept verbatim;
///   new-style ids must match `YYMM.NNNNN` (optionally with a `vN` version suffix).
/// * **PMID** — digits only (an optional `pmid:` label is stripped first); any other character
///   is rejected.
pub fn work_id_from_identifier(kind: WorkIdKind, value: &str) -> Option<String> {
    match kind {
        WorkIdKind::Doi => normalize_doi(value).map(|v| format!("doi:{v}")),
        WorkIdKind::Arxiv => normalize_arxiv(value).map(|v| format!("arxiv:{v}")),
        WorkIdKind::Pmid => normalize_pmid(value).map(|v| format!("pmid:{v}")),
    }
}

/// Scan free text for the first DOI / arXiv / PMID pattern and return its normalized WorkId.
///
/// A match is chosen by earliest position in `text`; ties are broken by precedence
/// DOI → arXiv → PMID. Old-style arXiv ids and PMIDs are only recognized behind their labels
/// (`arXiv:` / `PMID:`) to avoid matching arbitrary paths and numbers; new-style arXiv ids are
/// recognized bare.
pub fn detect_identifier(text: &str) -> Option<(WorkIdKind, String)> {
    let mut candidates: Vec<(usize, u8, WorkIdKind, String)> = Vec::new();

    // DOI: 10.<4+ digits>/<non-space>, trailing sentence punctuation trimmed.
    let doi_re = doi_regex();
    for m in doi_re.find_iter(text) {
        let raw = trim_doi_trailing(m.as_str());
        if let Some(id) = normalize_doi(raw) {
            candidates.push((m.start(), 0, WorkIdKind::Doi, format!("doi:{id}")));
        }
    }

    // arXiv: labeled (old or new style) or bare new style.
    let arxiv_labeled_re = arxiv_labeled_regex();
    for c in arxiv_labeled_re.captures_iter(text) {
        let cap = c.get(1).unwrap();
        if let Some(id) = normalize_arxiv(cap.as_str()) {
            let pos = c.get(0).unwrap().start();
            candidates.push((pos, 1, WorkIdKind::Arxiv, format!("arxiv:{id}")));
        }
    }
    let arxiv_bare_re = arxiv_bare_regex();
    for m in arxiv_bare_re.find_iter(text) {
        if let Some(id) = normalize_arxiv(m.as_str()) {
            candidates.push((m.start(), 1, WorkIdKind::Arxiv, format!("arxiv:{id}")));
        }
    }

    // PMID: only behind a label.
    let pmid_re = pmid_regex();
    for c in pmid_re.captures_iter(text) {
        let cap = c.get(1).unwrap();
        if let Some(id) = normalize_pmid(cap.as_str()) {
            let pos = c.get(0).unwrap().start();
            candidates.push((pos, 2, WorkIdKind::Pmid, format!("pmid:{id}")));
        }
    }

    candidates
        .into_iter()
        .min_by_key(|(pos, order, _, _)| (*pos, *order))
        .map(|(_, _, kind, id)| (kind, id))
}

// ---------------------------------------------------------------------------
// Normalizers
// ---------------------------------------------------------------------------

fn normalize_doi(value: &str) -> Option<String> {
    let mut s = value.trim();
    for prefix in [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
    ] {
        if let Some(rest) = strip_prefix_ci(s, prefix) {
            s = rest;
            break;
        }
    }
    if let Some(rest) = strip_prefix_ci(s, "doi:") {
        s = rest;
    }
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return None;
    }
    if doi_body_regex().is_match(&s) {
        Some(s)
    } else {
        None
    }
}

fn normalize_arxiv(value: &str) -> Option<String> {
    let mut s = value.trim();
    if let Some(rest) = strip_prefix_ci(s, "arxiv:") {
        s = rest.trim();
    }
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return None;
    }
    if s.contains('/') {
        // Old-style id, kept verbatim (e.g. `math.GT/0309136`).
        if arxiv_old_regex().is_match(s) {
            Some(s.to_string())
        } else {
            None
        }
    } else if arxiv_new_regex().is_match(s) {
        Some(s.to_string())
    } else {
        None
    }
}

fn normalize_pmid(value: &str) -> Option<String> {
    let mut s = value.trim();
    if let Some(rest) = strip_prefix_ci(s, "pmid:") {
        s = rest.trim();
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        Some(s.to_string())
    } else {
        None
    }
}

/// Case-insensitive prefix strip; returns the remainder when `s` starts with `prefix`.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Trim trailing sentence punctuation that commonly clings to a matched DOI.
fn trim_doi_trailing(s: &str) -> &str {
    s.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '>'])
}

// ---------------------------------------------------------------------------
// Compiled patterns (built once)
// ---------------------------------------------------------------------------

fn doi_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"10\.\d{4,}/\S+").unwrap())
}

fn doi_body_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^10\.\d{4,}/\S+$").unwrap())
}

/// Old-style arXiv: `archive[.category]/NNNNNNN` (7 digits), e.g. `math.GT/0309136`.
fn arxiv_old_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z][a-z\-]*(?:\.[A-Za-z\-]+)?/\d{7}$").unwrap())
}

/// New-style arXiv: `YYMM.NNNNN`, 4-5 digits, optional `vN` version suffix.
fn arxiv_new_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{4}\.\d{4,5}(?:v\d+)?$").unwrap())
}

/// Labeled arXiv (old or new style) inside free text.
fn arxiv_labeled_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)arxiv[:\s]\s*([a-z][a-z\-]*(?:\.[A-Za-z\-]+)?/\d{7}|\d{4}\.\d{4,5}(?:v\d+)?)")
            .unwrap()
    })
}

/// Bare new-style arXiv id (word-bounded so it doesn't grab digits inside a larger token).
fn arxiv_bare_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b\d{4}\.\d{4,5}(?:v\d+)?\b").unwrap())
}

/// Labeled PMID inside free text.
fn pmid_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)pmid[:#\s]*(\d+)").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doi_normalization() {
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Doi, "10.1234/AbC"),
            Some("doi:10.1234/abc".into())
        );
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Doi, "https://doi.org/10.1234/XyZ"),
            Some("doi:10.1234/xyz".into())
        );
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Doi, "  doi:10.5555/ABC.1  "),
            Some("doi:10.5555/abc.1".into())
        );
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Doi, "HTTP://DX.DOI.ORG/10.1234/Ab"),
            Some("doi:10.1234/ab".into())
        );
        // Rejections.
        assert_eq!(work_id_from_identifier(WorkIdKind::Doi, ""), None);
        assert_eq!(work_id_from_identifier(WorkIdKind::Doi, "not-a-doi"), None);
        assert_eq!(work_id_from_identifier(WorkIdKind::Doi, "10.12/x"), None); // <4 digits
        assert_eq!(work_id_from_identifier(WorkIdKind::Doi, "10.1234/"), None); // empty suffix
        assert_eq!(work_id_from_identifier(WorkIdKind::Doi, "10.1234/a b"), None); // space
    }

    #[test]
    fn arxiv_normalization() {
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Arxiv, "2301.00001"),
            Some("arxiv:2301.00001".into())
        );
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Arxiv, "arXiv:2301.00001"),
            Some("arxiv:2301.00001".into())
        );
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Arxiv, "arxiv:2301.00001v3"),
            Some("arxiv:2301.00001v3".into())
        );
        // Old-style kept verbatim (case preserved).
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Arxiv, "arXiv:math.GT/0309136"),
            Some("arxiv:math.GT/0309136".into())
        );
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Arxiv, "  hep-th/9901001 "),
            Some("arxiv:hep-th/9901001".into())
        );
        // Rejections.
        assert_eq!(work_id_from_identifier(WorkIdKind::Arxiv, ""), None);
        assert_eq!(work_id_from_identifier(WorkIdKind::Arxiv, "12345"), None);
        assert_eq!(work_id_from_identifier(WorkIdKind::Arxiv, "2301.1"), None);
        assert_eq!(work_id_from_identifier(WorkIdKind::Arxiv, "math.GT/123"), None); // !=7 digits
        assert_eq!(work_id_from_identifier(WorkIdKind::Arxiv, "arXiv:"), None);
    }

    #[test]
    fn pmid_normalization() {
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Pmid, "12345678"),
            Some("pmid:12345678".into())
        );
        assert_eq!(
            work_id_from_identifier(WorkIdKind::Pmid, "  PMID:998877  "),
            Some("pmid:998877".into())
        );
        // Rejections.
        assert_eq!(work_id_from_identifier(WorkIdKind::Pmid, ""), None);
        assert_eq!(work_id_from_identifier(WorkIdKind::Pmid, "abc"), None);
        assert_eq!(work_id_from_identifier(WorkIdKind::Pmid, "12a45"), None);
        assert_eq!(work_id_from_identifier(WorkIdKind::Pmid, "12 45"), None);
    }

    #[test]
    fn detect_precedence_by_position() {
        // arXiv appears first → arXiv wins even though a DOI follows.
        let t = "see arXiv:2301.00001 then doi:10.1234/xyz";
        assert_eq!(
            detect_identifier(t),
            Some((WorkIdKind::Arxiv, "arxiv:2301.00001".into()))
        );

        // DOI appears first → DOI wins.
        let t2 = "doi:10.1234/xyz and arXiv:2301.00001";
        assert_eq!(
            detect_identifier(t2),
            Some((WorkIdKind::Doi, "doi:10.1234/xyz".into()))
        );

        // Bare new-style arXiv.
        let t3 = "preprint 2301.00001v2 available";
        assert_eq!(
            detect_identifier(t3),
            Some((WorkIdKind::Arxiv, "arxiv:2301.00001v2".into()))
        );

        // PMID behind its label.
        let t4 = "PubMed PMID: 12345678.";
        assert_eq!(
            detect_identifier(t4),
            Some((WorkIdKind::Pmid, "pmid:12345678".into()))
        );
    }

    #[test]
    fn detect_doi_strips_trailing_punctuation() {
        let t = "Available at https://doi.org/10.1145/3292500.3330701.";
        assert_eq!(
            detect_identifier(t),
            Some((WorkIdKind::Doi, "doi:10.1145/3292500.3330701".into()))
        );
    }

    #[test]
    fn detect_no_match() {
        assert_eq!(detect_identifier(""), None);
        assert_eq!(detect_identifier("just some ordinary prose, no ids here"), None);
        assert_eq!(detect_identifier("version 2 of the report"), None);
    }
}
