//! Streaming RSS 2.0 / Atom 1.0 feed parsing via quick-xml events.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::dates;
use crate::{DiscoveredResource, DiscoverError, ResourceKind, MAX_RESOURCES};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Unknown,
    Rss,
    Atom,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Title,
    Link,
    GuidOrId,
    PubDate,
    Published,
    Updated,
}

#[derive(Default)]
struct Entry {
    title: Option<String>,
    link: Option<String>,
    guid: Option<String>,
    pubdate: Option<String>,
    published: Option<String>,
    updated: Option<String>,
}

impl Entry {
    fn finish(self, source_url: &str, mode: Mode) -> Option<DiscoveredResource> {
        let url = self
            .link
            .filter(|s| !s.is_empty())
            .or_else(|| self.guid.clone().filter(|g| url::Url::parse(g).is_ok()))?;
        let published_at = match mode {
            Mode::Rss => self.pubdate.as_deref().and_then(dates::parse_rfc822),
            Mode::Atom => self
                .published
                .as_deref()
                .and_then(dates::parse_rfc3339)
                .or_else(|| self.updated.as_deref().and_then(dates::parse_rfc3339)),
            Mode::Unknown => None,
        };
        Some(DiscoveredResource {
            url: resolve(source_url, url.trim()),
            lastmod: None,
            changefreq: None,
            priority: None,
            kind: ResourceKind::FeedEntry,
            title: self.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
            published_at,
            source_url: source_url.to_string(),
        })
    }
}

/// Parse an (already decompressed) RSS or Atom feed body. `source_url` is the feed's
/// own URL, used to resolve relative item links.
pub(crate) fn parse(xml: &[u8], source_url: &str) -> Result<Vec<DiscoveredResource>, DiscoverError> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();

    let mut mode = Mode::Unknown;
    let mut in_entry = false;
    let mut entry = Entry::default();
    let mut field = Field::None;
    let mut text = String::new();
    let mut out: Vec<DiscoveredResource> = Vec::new();

    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| DiscoverError::Xml(e.to_string()))?;
        match ev {
            Event::Start(ref el) => {
                let name = el.local_name();
                match name.as_ref() {
                    b"rss" => mode = Mode::Rss,
                    b"feed" => mode = Mode::Atom,
                    b"item" if mode == Mode::Rss => {
                        in_entry = true;
                        entry = Entry::default();
                    }
                    b"entry" if mode == Mode::Atom => {
                        in_entry = true;
                        entry = Entry::default();
                    }
                    b"title" if in_entry => field = Field::Title,
                    b"link" if in_entry => {
                        if mode == Mode::Atom {
                            if let Some(href) = attr(el, b"href") {
                                entry.link = Some(href);
                            }
                            field = Field::None;
                        } else {
                            field = Field::Link;
                        }
                    }
                    b"guid" if in_entry => field = Field::GuidOrId,
                    b"id" if in_entry => field = Field::GuidOrId,
                    b"pubDate" if in_entry => field = Field::PubDate,
                    b"published" if in_entry => field = Field::Published,
                    b"updated" if in_entry => field = Field::Updated,
                    _ => field = Field::None,
                }
                text.clear();
            }
            Event::Empty(ref el) => {
                let name = el.local_name();
                if name.as_ref() == b"link" && in_entry {
                    if let Some(href) = attr(el, b"href") {
                        entry.link = Some(href);
                    }
                }
                field = Field::None;
                text.clear();
            }
            Event::Text(ref t) => {
                let s = t.unescape().map_err(|e| DiscoverError::Xml(e.to_string()))?;
                text.push_str(&s);
            }
            Event::CData(ref t) => {
                text.push_str(&String::from_utf8_lossy(t.as_ref()));
            }
            Event::End(ref el) => {
                let name = el.local_name();
                match name.as_ref() {
                    b"item" | b"entry" => {
                        if in_entry {
                            if let Some(r) = std::mem::take(&mut entry).finish(source_url, mode) {
                                out.push(r);
                            }
                        }
                        in_entry = false;
                    }
                    b"title" if field == Field::Title => entry.title = Some(text.clone()),
                    b"link" if field == Field::Link => entry.link = Some(text.clone()),
                    b"guid" | b"id" if field == Field::GuidOrId => entry.guid = Some(text.clone()),
                    b"pubDate" if field == Field::PubDate => entry.pubdate = Some(text.clone()),
                    b"published" if field == Field::Published => entry.published = Some(text.clone()),
                    b"updated" if field == Field::Updated => entry.updated = Some(text.clone()),
                    _ => {}
                }
                text.clear();
                field = Field::None;
                if out.len() >= MAX_RESOURCES {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if mode == Mode::Unknown && out.is_empty() {
        return Err(DiscoverError::Unsupported("not an RSS or Atom feed".into()));
    }
    out.truncate(MAX_RESOURCES);
    Ok(out)
}

fn attr(el: &BytesStart, key: &[u8]) -> Option<String> {
    for a in el.attributes().flatten() {
        if a.key.local_name().as_ref() == key {
            return Some(String::from_utf8_lossy(&a.value).to_string());
        }
    }
    None
}

fn resolve(source_url: &str, url: &str) -> String {
    match url::Url::parse(source_url) {
        Ok(base) => base
            .join(url)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| url.to_string()),
        Err(_) => url.to_string(),
    }
}
