//! Streaming sitemap / sitemap-index parsing via quick-xml events.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::{DiscoveredResource, DiscoverError, ResourceKind, MAX_RESOURCES};

#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Loc,
    Lastmod,
    Changefreq,
    Priority,
}

#[derive(Default)]
struct Entry {
    loc: Option<String>,
    lastmod: Option<String>,
    changefreq: Option<String>,
    priority: Option<f32>,
}

/// Parse a (already decompressed) sitemap body. `source_url` is the sitemap's own URL,
/// used to resolve relative `<loc>` values.
pub(crate) fn parse(xml: &[u8], source_url: &str) -> Result<Vec<DiscoveredResource>, DiscoverError> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();

    let mut is_index = false;
    let mut saw_root = false;
    let mut entry: Option<Entry> = None;
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
                    b"urlset" => {
                        is_index = false;
                        saw_root = true;
                    }
                    b"sitemapindex" => {
                        is_index = true;
                        saw_root = true;
                    }
                    b"url" | b"sitemap" => entry = Some(Entry::default()),
                    b"loc" if entry.is_some() => field = Field::Loc,
                    b"lastmod" if entry.is_some() => field = Field::Lastmod,
                    b"changefreq" if entry.is_some() => field = Field::Changefreq,
                    b"priority" if entry.is_some() => field = Field::Priority,
                    _ => field = Field::None,
                }
                text.clear();
            }
            Event::Empty(ref el) => {
                let name = el.local_name();
                if matches!(name.as_ref(), b"url" | b"sitemap") {
                    entry = Some(Entry::default());
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
                    b"url" | b"sitemap" => {
                        if let Some(e) = entry.take() {
                            if let Some(loc) = e.loc {
                                let kind = if is_index {
                                    ResourceKind::Sitemap
                                } else {
                                    ResourceKind::Page
                                };
                                out.push(DiscoveredResource {
                                    url: resolve(source_url, loc.trim()),
                                    lastmod: clean(e.lastmod),
                                    changefreq: clean(e.changefreq),
                                    priority: e.priority,
                                    kind,
                                    title: None,
                                    published_at: None,
                                    source_url: source_url.to_string(),
                                });
                            }
                        }
                        field = Field::None;
                    }
                    b"loc" if field == Field::Loc => {
                        if let Some(e) = entry.as_mut() {
                            e.loc = Some(text.clone());
                        }
                    }
                    b"lastmod" if field == Field::Lastmod => {
                        if let Some(e) = entry.as_mut() {
                            e.lastmod = Some(text.clone());
                        }
                    }
                    b"changefreq" if field == Field::Changefreq => {
                        if let Some(e) = entry.as_mut() {
                            e.changefreq = Some(text.clone());
                        }
                    }
                    b"priority" if field == Field::Priority => {
                        if let Some(e) = entry.as_mut() {
                            e.priority = text.trim().parse::<f32>().ok();
                        }
                    }
                    _ => {}
                }
                text.clear();
                if out.len() >= MAX_RESOURCES {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !saw_root && out.is_empty() {
        return Err(DiscoverError::Unsupported("not a sitemap or sitemap index".into()));
    }
    out.truncate(MAX_RESOURCES);
    Ok(out)
}

fn clean(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Resolve a possibly-relative `<loc>` against the source sitemap URL.
fn resolve(source_url: &str, loc: &str) -> String {
    match url::Url::parse(source_url) {
        Ok(base) => base
            .join(loc)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| loc.to_string()),
        Err(_) => loc.to_string(),
    }
}
