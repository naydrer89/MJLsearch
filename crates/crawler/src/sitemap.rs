//! Sitemap parsing.
//!
//! A sitemap is the one document on a site that states its URLs outright, which
//! is why every search engine reads them: a link graph only exposes what pages
//! choose to link to, and the pages nobody links to are exactly the ones a
//! search index is missing.
//!
//! The parser is a scanner rather than a tree builder. Sitemaps are a flat
//! sequence of records in a namespace, the only element that matters is `<loc>`,
//! and a general XML parser would materialise a 50,000-entry document into a DOM
//! to read one field out of each entry. Scanning also means a sitemap with one
//! malformed record still yields every other URL in it, where a strict parser
//! would fail the whole file.

use crate::parser::decode_entities;

/// Extracts every URL from a sitemap or sitemap index body.
///
/// Both document kinds use `<loc>`, so one scanner serves both: in a sitemap the
/// values are pages, in a sitemap index they are further sitemaps. The caller
/// does not need to know which, because a fetched sitemap is recognised as one
/// on the way back in.
pub fn parse(body: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = body;

    while let Some(opening) = find_ignore_case(rest, "<loc") {
        let after_open = &rest[opening + "<loc".len()..];
        let Some(tag_end) = after_open.find('>') else {
            break;
        };
        let content = &after_open[tag_end + 1..];

        let Some(closing) = find_ignore_case(content, "</loc") else {
            break;
        };
        let raw = &content[..closing];
        // Resume past the closing tag, so a `<loc>` nested in the text of
        // another cannot be read twice.
        rest = &content[closing..];

        if let Some(url) = normalize(raw) {
            urls.push(url);
        }
    }

    urls
}

/// Whether a sitemap body is an index pointing at further sitemaps.
///
/// Worth knowing separately because depth matters: following an index is
/// progress, whereas following a page of URLs is the end of the trail, and a
/// crawler that cannot tell them apart has no way to bound the first without
/// also cutting off the second.
pub fn is_index(body: &str) -> bool {
    find_ignore_case(body, "<sitemapindex").is_some()
}

/// Cleans one `<loc>` value into a fetchable absolute URL.
fn normalize(raw: &str) -> Option<String> {
    let text = strip_cdata(raw.trim());

    // The entity decoding matters: sitemaps are the one place where `&amp;` in a
    // URL is near-universal, because an unescaped ampersand would make the
    // document invalid.
    let decoded = decode_entities(text);
    let parsed = url::Url::parse(decoded.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some(parsed.to_string())
}

/// Unwraps a `CDATA` section, leaving anything else untouched.
fn strip_cdata(value: &str) -> &str {
    match value.strip_prefix("<![CDATA[") {
        Some(inner) => inner.strip_suffix("]]>").unwrap_or(inner),
        None => value,
    }
}

/// Byte index of `needle` in `haystack`, ignoring ASCII case.
///
/// Hand-written because `to_lowercase()` allocates a second copy of a document
/// that may be megabytes, for a search that only ever needs to be ASCII
/// case-insensitive -- XML tag names are ASCII by definition.
fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }

    (0..=haystack.len() - needle.len()).find(|&start| {
        haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_sitemap_yields_its_urls() {
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
            <url><loc>https://example.com/</loc><lastmod>2024-01-01</lastmod></url>\
            <url><loc>https://example.com/about</loc></url>\
            </urlset>";

        assert_eq!(
            parse(body),
            vec![
                "https://example.com/".to_string(),
                "https://example.com/about".to_string()
            ]
        );
    }

    #[test]
    fn a_sitemap_index_yields_the_nested_sitemaps() {
        let body = "<sitemapindex><sitemap><loc>https://example.com/a.xml</loc></sitemap>\
            <sitemap><loc>https://example.com/b.xml</loc></sitemap></sitemapindex>";

        assert_eq!(
            parse(body),
            vec![
                "https://example.com/a.xml".to_string(),
                "https://example.com/b.xml".to_string()
            ]
        );
        assert!(is_index(body));
    }

    #[test]
    fn an_ordinary_sitemap_is_not_an_index() {
        assert!(!is_index(
            "<urlset><url><loc>https://a.example/</loc></url></urlset>"
        ));
    }

    #[test]
    fn a_cdata_wrapped_url_is_unwrapped() {
        let body = "<urlset><url><loc><![CDATA[https://example.com/x]]></loc></url></urlset>";
        assert_eq!(parse(body), vec!["https://example.com/x".to_string()]);
    }

    #[test]
    fn an_escaped_ampersand_in_a_url_is_decoded() {
        // Without this the enqueued URL carries a literal `&amp;` and no server
        // would recognise the query string.
        let body = "<urlset><url><loc>https://example.com/s?x=1&amp;y=2</loc></url></urlset>";
        assert_eq!(
            parse(body),
            vec!["https://example.com/s?x=1&y=2".to_string()]
        );
    }

    #[test]
    fn surrounding_whitespace_and_newlines_are_tolerated() {
        let body = "<urlset><url><loc>\n   https://example.com/x\n </loc></url></urlset>";
        assert_eq!(parse(body), vec!["https://example.com/x".to_string()]);
    }

    #[test]
    fn tag_case_does_not_matter() {
        let body = "<urlset><URL><LOC>https://example.com/x</LOC></URL></urlset>";
        assert_eq!(parse(body), vec!["https://example.com/x".to_string()]);
    }

    #[test]
    fn entries_that_are_not_fetchable_urls_are_skipped_not_fatal() {
        let body = "<urlset>\
            <url><loc>mailto:someone@example.com</loc></url>\
            <url><loc>ftp://example.com/f</loc></url>\
            <url><loc></loc></url>\
            <url><loc>https://example.com/kept</loc></url>\
            </urlset>";

        // One bad record must not cost the whole sitemap.
        assert_eq!(parse(body), vec!["https://example.com/kept".to_string()]);
    }

    #[test]
    fn a_truncated_document_returns_what_it_has_instead_of_nothing() {
        // The stream ended inside the second record. Everything before it is
        // still good, and a crawler that threw the whole file away would lose
        // the URLs of a large sitemap to a single dropped connection.
        let body = "<urlset><url><loc>https://example.com/one</loc></url>\
            <url><loc>https://example.com/tw";
        assert_eq!(parse(body), vec!["https://example.com/one".to_string()]);
    }

    #[test]
    fn a_document_with_no_locations_is_empty_rather_than_wrong() {
        assert!(parse("").is_empty());
        assert!(parse("<html><body>not a sitemap</body></html>").is_empty());
        assert!(parse("<urlset></urlset>").is_empty());
    }

    #[test]
    fn urls_are_not_read_twice_when_a_loc_is_nested_in_text() {
        let body = "<urlset><url><loc>https://example.com/a</loc></url>\
            <url><loc>https://example.com/b</loc></url></urlset>";
        assert_eq!(parse(body).len(), 2);
    }
}
