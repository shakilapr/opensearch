//! OAI-PMH adapter.
//!
//! Harvests `OAI-PMH` ListRecords responses from an injected fixture **directory**. The first
//! request (cursor `None`) reads `initial.xml`; each subsequent request reads `<token>.xml`,
//! where `<token>` is the `resumptionToken` returned in the *previous* page. This mirrors the
//! protocol exactly (the token is opaque and round-tripped by the caller) while staying fully
//! offline — a live endpoint would substitute the token into a request URL, but no network call
//! is made here.
//!
//! Records are built from the `oai_dc` metadata: `dc:title`, a URL-shaped `dc:identifier`, and
//! `dc:description`; the header `datestamp` becomes `published_at`.

use std::path::PathBuf;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::adapters::read_bounded;
use crate::time::parse_epoch;
use crate::{bound_text, DatasetAdapter, DatasetError, Page, Record, MAX_RECORDS};

const INITIAL_FILE: &str = "initial.xml";
const MAX_RECORD_TEXT: usize = crate::MAX_RECORD_TEXT_BYTES;

/// Parses OAI-PMH ListRecords XML, following `resumptionToken` across pages.
pub struct OaiPmhAdapter {
    dir: PathBuf,
    source: String,
}

impl OaiPmhAdapter {
    /// Adapter over a fixture directory containing `initial.xml` and one XML file per token.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        OaiPmhAdapter {
            dir: dir.into(),
            source: "oai-pmh".to_string(),
        }
    }

    /// Path of the fixture file for a given cursor (token filenames are sanitized).
    fn file_for(&self, cursor: Option<&str>) -> PathBuf {
        match cursor {
            None => self.dir.join(INITIAL_FILE),
            Some(token) => self.dir.join(format!("{}.xml", sanitize_token(token))),
        }
    }
}

impl DatasetAdapter for OaiPmhAdapter {
    fn name(&self) -> &str {
        &self.source
    }

    fn page(&mut self, cursor: Option<&str>) -> Result<Page, DatasetError> {
        let path = self.file_for(cursor);
        let bytes = read_bounded(&path)?;
        let (records, token) = parse_list_records(&bytes, &self.source)?;
        Ok(Page {
            records,
            next: token,
        })
    }
}

/// Keep only filename-safe characters from a resumption token.
fn sanitize_token(token: &str) -> String {
    let cleaned: String = token
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "token".to_string()
    } else {
        cleaned
    }
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Field {
    #[default]
    None,
    HeaderIdentifier,
    Datestamp,
    Title,
    DcIdentifier,
    Description,
    Date,
    Token,
}

#[derive(Default, Clone)]
struct Draft {
    identifier: String,
    datestamp: Option<String>,
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    date: Option<String>,
}

/// Parse one `ListRecords` document into records plus its (trimmed, non-empty) resumption token.
fn parse_list_records(xml: &[u8], source: &str) -> Result<(Vec<Record>, Option<String>), DatasetError> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();

    let mut in_record = false;
    let mut in_header = false;
    let mut in_metadata = false;
    let mut field = Field::None;
    let mut text = String::new();
    let mut draft = Draft::default();
    let mut token: Option<String> = None;
    let mut out: Vec<Record> = Vec::new();

    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| DatasetError::Parse(format!("oai-pmh xml: {e}")))?;
        match ev {
            Event::Start(ref el) => {
                begin_element(el, &mut in_record, &mut in_header, &mut in_metadata, &mut field, &mut draft);
                text.clear();
            }
            Event::Empty(ref el) => {
                // A self-closing <resumptionToken/> means "no more pages".
                if el.local_name().as_ref() == b"resumptionToken" {
                    token = None;
                }
                begin_element(el, &mut in_record, &mut in_header, &mut in_metadata, &mut field, &mut draft);
                text.clear();
            }
            Event::Text(ref t) => {
                let s = t
                    .unescape()
                    .map_err(|e| DatasetError::Parse(format!("oai-pmh text: {e}")))?;
                if text.len() < MAX_RECORD_TEXT {
                    text.push_str(&s);
                }
            }
            Event::CData(ref t) => {
                if text.len() < MAX_RECORD_TEXT {
                    text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Event::End(ref el) => {
                let name = el.local_name();
                let trimmed = text.trim().to_string();
                match name.as_ref() {
                    b"record" => {
                        if in_record {
                            out.push(draft.clone().finish(source));
                            draft = Draft::default();
                        }
                        in_record = false;
                    }
                    b"header" => in_header = false,
                    b"metadata" => in_metadata = false,
                    b"identifier" if field == Field::HeaderIdentifier => draft.identifier = trimmed,
                    b"datestamp" if field == Field::Datestamp => draft.datestamp = Some(trimmed),
                    b"title" if field == Field::Title => draft.title = Some(trimmed),
                    b"identifier" if field == Field::DcIdentifier => {
                        if draft.url.is_none() && looks_like_url(&trimmed) {
                            draft.url = Some(trimmed);
                        }
                    }
                    b"description" if field == Field::Description => draft.description = Some(trimmed),
                    b"date" if field == Field::Date => draft.date = Some(trimmed),
                    b"resumptionToken" if field == Field::Token => {
                        token = if trimmed.is_empty() { None } else { Some(trimmed) };
                    }
                    _ => {}
                }
                field = Field::None;
                text.clear();
                if out.len() >= MAX_RECORDS {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok((out, token))
}

/// Update nesting + field state from a start/empty element.
fn begin_element(
    el: &BytesStart,
    in_record: &mut bool,
    in_header: &mut bool,
    in_metadata: &mut bool,
    field: &mut Field,
    draft: &mut Draft,
) {
    let name = el.local_name();
    *field = match name.as_ref() {
        b"record" => {
            *in_record = true;
            *draft = Draft::default();
            Field::None
        }
        b"header" => {
            *in_header = true;
            Field::None
        }
        b"metadata" => {
            *in_metadata = true;
            Field::None
        }
        b"identifier" if *in_record && *in_header => Field::HeaderIdentifier,
        b"datestamp" if *in_record && *in_header => Field::Datestamp,
        b"title" if *in_metadata => Field::Title,
        b"identifier" if *in_metadata => Field::DcIdentifier,
        b"description" if *in_metadata => Field::Description,
        b"date" if *in_metadata => Field::Date,
        b"resumptionToken" => Field::Token,
        _ => Field::None,
    };
}

impl Draft {
    fn finish(self, source: &str) -> Record {
        let id = if self.identifier.is_empty() {
            self.url
                .clone()
                .unwrap_or_else(|| format!("{source}:{}", self.title.clone().unwrap_or_default()))
        } else {
            self.identifier
        };
        let mut record = Record::new(id, source);
        record.url = self.url;
        record.title = self.title.clone();
        record.text = bound_text(&self.description.or(self.title).unwrap_or_default());
        record.published_at = self
            .datestamp
            .as_deref()
            .and_then(parse_epoch)
            .or_else(|| self.date.as_deref().and_then(parse_epoch));
        if let Some(ds) = self.datestamp {
            record.extras.insert("datestamp".to_string(), ds);
        }
        record
    }
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}
