//! Crawl policy: which URLs are worth fetching, and in what form.
//!
//! Two decisions live here, and both exist because a real site is not a graph of
//! distinct documents. It is a graph where the *same* document appears under
//! many URLs -- campaign tags, session ids, click ids -- and where most of the
//! links on a page point at something that is not a document at all.
//!
//! * [`canonical_url`] folds those duplicates together, so the seen-URL filter
//!   actually suppresses them instead of letting every variant buy its own
//!   fetch, its own WARC record and its own near-identical index entry.
//! * [`CrawlScope`] keeps a crawl of one site on that site. A search engine that
//!   is given `github.com` indexes github.com; a crawler that follows every link
//!   off the homepage spends its whole budget on the rest of the web and indexes
//!   almost none of the site it was pointed at.

use std::collections::HashSet;

use crate::frontier::host_of;

/// Query parameters that identify a campaign or a session rather than a
/// document, so a URL carrying them is not a page we have not seen.
///
/// Matched either exactly or by the prefixes in [`TRACKING_PREFIXES`]. Only
/// parameters that are known to be non-semantic are listed: a list that guessed
/// would drop real pages, and a dropped page is invisible, where an extra fetch
/// is merely wasteful.
const TRACKING_PARAMS: &[&str] = &[
    "fbclid",
    "gclid",
    "gclsrc",
    "dclid",
    "msclkid",
    "yclid",
    "twclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "s_kwcid",
    "vero_conv",
    "vero_id",
    "wickedid",
    "_openstat",
    "_ga",
    "_gl",
    "ef_id",
    "spm",
    "scm",
    // Named in the singular because these are the exact spellings the three
    // biggest sites in the index use: Amazon decorates nearly every internal
    // link with `ref_=nav_*`, and Google's own pages carry `ucbcb`.
    "ucbcb",
    "ucpp",
    "primecampaignid",
    "gsc",
];

/// Parameter-name prefixes that mark a campaign parameter.
const TRACKING_PREFIXES: &[&str] = &[
    "utm_", "pk_", "mtm_", "hsa_",
    // `ref_` covers Amazon's `ref_=nav_cs_books` family, which is the single
    // largest source of one-document-many-URLs in a crawl of amazon.com.
    "ref_",
    // Amazon's recommendation carousel tracking, which is attached to most of
    // the "customers also viewed" links on a product page.
    "pf_rd_", "pd_rd_",
    // Analytics cross-domain decoration. The exact names are listed rather than a
    // prefix, because `_ga` as a prefix would also swallow a parameter called
    // `_gallery`, and a dropped parameter is invisible where a kept one is free.
    "_gac_",
];

/// File extensions that are never an HTML page.
///
/// Checked before a URL is enqueued rather than after it is fetched, because the
/// cost of a wrong guess is asymmetric: skipping a real page loses it silently,
/// so the list is deliberately short and contains only unambiguous data formats.
/// Anything not on it is still fetched and then judged by its `Content-Type`.
const NON_PAGE_EXTENSIONS: &[&str] = &[
    // images
    "jpg", "jpeg", "png", "gif", "webp", "svg", "ico", "bmp", "tif", "tiff", "avif", "heic",
    // stylesheets and code
    "css", "js", "mjs", "map", "wasm", // fonts
    "woff", "woff2", "ttf", "otf", "eot", // media
    "mp3", "mp4", "m4a", "m4v", "wav", "ogg", "oga", "ogv", "webm", "avi", "mov", "mkv", "flv",
    // archives and binaries
    "zip", "tar", "rar", "7z", "bz2", "xz", "exe", "dmg", "deb", "rpm", "apk", "iso", "bin",
    // office and print documents
    "pdf", "ps", "eps", "csv", "tsv", "xls", "xlsx", "doc", "docx", "ppt", "pptx", "odt", "ods",
];

/// Whether a parameter name is a campaign or session marker.
pub fn is_tracking_param(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    TRACKING_PARAMS.contains(&lowered.as_str())
        || TRACKING_PREFIXES
            .iter()
            .any(|prefix| lowered.starts_with(prefix))
}

/// The canonical, fetchable form of a URL, or `None` if it is not one.
///
/// Applied to every URL that enters the frontier, so the deduplication does its
/// job on the form that actually identifies the document. Returns `None` for
/// anything that could never be fetched -- another scheme, an unparsable string
/// -- which lets callers drop it instead of enqueuing a guaranteed failure.
pub fn canonical_url(raw: &str) -> Option<String> {
    let mut parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    // The `url` crate already folds case in the host and drops a default port,
    // so the only work left here is the fragment and the campaign parameters.
    parsed.set_fragment(None);
    strip_tracking_params(&mut parsed);
    Some(parsed.to_string())
}

/// Removes campaign parameters, leaving the rest of the query byte for byte.
///
/// The rewrite only happens when something was actually removed. Re-serialising
/// an untouched query would re-encode separators and character escapes, and two
/// spellings of the same query would then be counted as two documents.
fn strip_tracking_params(parsed: &mut url::Url) {
    let Some(query) = parsed.query() else {
        return;
    };

    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let kept: Vec<&(String, String)> = pairs
        .iter()
        .filter(|(key, _)| !is_tracking_param(key))
        .collect();

    if kept.len() == pairs.len() {
        return;
    }
    if kept.is_empty() {
        parsed.set_query(None);
        return;
    }

    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in kept {
        serializer.append_pair(key, value);
    }
    parsed.set_query(Some(&serializer.finish()));
}

/// Whether a URL plausibly addresses an HTML document.
///
/// A sitemap is the exception that keeps this from being a pure extension test:
/// `sitemap.xml.gz` is a document we very much want, and a `gz` entry in the
/// list above would silently drop every compressed sitemap on the web.
pub fn is_probably_page(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };

    let segment = parsed
        .path()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let segment = segment.strip_suffix(".gz").unwrap_or(&segment);

    match segment.rsplit_once('.') {
        // A leading dot with no stem (`.htaccess`) is a filename, not a suffix.
        Some((stem, extension)) if !stem.is_empty() => !NON_PAGE_EXTENSIONS.contains(&extension),
        _ => true,
    }
}

/// Whether a URL addresses a sitemap rather than a page.
///
/// Only ever consulted for a URL we deliberately fetched as a sitemap or that
/// the server answered with an XML content type, because a sitemap is a list of
/// URLs rather than a document to index.
pub fn is_sitemap_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let path = parsed.path().to_ascii_lowercase();
    path.ends_with("sitemap.xml") || path.ends_with(".xml") || path.ends_with(".xml.gz")
}

/// Whether a server answered with XML that is not HTML.
pub fn is_xml_content_type(content_type: Option<&str>) -> bool {
    match content_type {
        None => false,
        Some(value) => {
            let value = value.to_ascii_lowercase();
            value.contains("xml") && !value.contains("html")
        }
    }
}

/// The set of hosts a crawl is allowed to fetch from.
///
/// Membership is exact, and deliberately not "the host or any subdomain": a
/// subdomain is a different site operated by the same organisation, often with
/// its own robots.txt and its own rate limits, and treating it as one site would
/// apply one site's politeness budget to another's servers. `docs.github.com`
/// therefore has to be named, either as a seed or with `--allow-host`.
#[derive(Debug, Clone, Default)]
pub struct CrawlScope {
    hosts: HashSet<String>,
    restricted: bool,
}

impl CrawlScope {
    /// A scope that permits every host.
    pub fn unrestricted() -> Self {
        Self::default()
    }

    /// Builds a scope from seeds and explicitly allowed hosts.
    ///
    /// `restricted` is the operator's `--stay-on-site`; naming a host with
    /// `--allow-host` implies it, because an allowlist with nothing to restrict
    /// would be a confusing way to say nothing.
    pub fn new(restricted: bool, seeds: &[String], allowed_hosts: &[String]) -> Self {
        let mut hosts: HashSet<String> = HashSet::new();
        for seed in seeds {
            if let Some(host) = host_of(seed) {
                hosts.insert(host);
            }
        }
        for host in allowed_hosts {
            hosts.insert(host.trim().to_ascii_lowercase());
        }

        Self {
            restricted: restricted || !hosts.is_empty(),
            hosts,
        }
    }

    /// Whether this scope refuses any host at all.
    pub fn is_restricted(&self) -> bool {
        self.restricted
    }

    /// The hosts in scope, sorted, for reporting.
    pub fn hosts(&self) -> Vec<&str> {
        let mut hosts: Vec<&str> = self.hosts.iter().map(String::as_str).collect();
        hosts.sort_unstable();
        hosts
    }

    /// Whether a URL is inside the crawl's scope.
    pub fn allows(&self, url: &str) -> bool {
        if !self.restricted {
            return true;
        }
        match host_of(url) {
            Some(host) => self.hosts.contains(&host),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_campaign_parameter_is_stripped_and_the_rest_survives() {
        assert_eq!(
            canonical_url("https://example.com/p?utm_source=news&id=7&utm_medium=email").as_deref(),
            Some("https://example.com/p?id=7")
        );
    }

    #[test]
    fn a_url_whose_only_parameters_are_campaign_markers_loses_the_question_mark() {
        // `/?utm_source=x` and `/` are one document. Leaving the empty `?` in
        // place would keep them apart and fetch the page twice.
        assert_eq!(
            canonical_url("https://example.com/p?fbclid=abc").as_deref(),
            Some("https://example.com/p")
        );
    }

    #[test]
    fn an_untouched_query_is_not_re_encoded() {
        // The trap: rebuilding every query would turn `a=b,c` into `a=b%2Cc` and
        // make a stable URL look like a new one on the second sighting.
        let url = "https://example.com/s?q=a+b,c&x=1";
        assert_eq!(canonical_url(url).as_deref(), Some(url));
    }

    #[test]
    fn semantic_parameters_are_never_dropped() {
        for url in [
            "https://example.com/s?page=2",
            "https://example.com/s?q=rust&lang=de",
            "https://example.com/item?id=12345",
            "https://example.com/s?ref=sidebar",
            "https://example.com/s?hl=de",
        ] {
            assert_eq!(canonical_url(url).as_deref(), Some(url), "{url}");
        }
    }

    #[test]
    fn the_referral_parameters_the_big_sites_actually_use_are_stripped() {
        // Measured against real pages rather than guessed at: every one of these
        // appeared in a crawl of amazon.com or google.com, and each made the same
        // document look like a different one.
        assert_eq!(
            canonical_url("https://www.amazon.com/amazonprime?ref_=nav_cs_primelink_nonmember")
                .as_deref(),
            Some("https://www.amazon.com/amazonprime")
        );
        assert_eq!(
            canonical_url("https://www.google.com/finance/sitemap.xml?ucbcb=1").as_deref(),
            Some("https://www.google.com/finance/sitemap.xml")
        );
        assert_eq!(
            canonical_url("https://example.com/x?ref_src=twsrc%5Etfw&id=9").as_deref(),
            Some("https://example.com/x?id=9")
        );
    }

    #[test]
    fn the_second_round_of_measured_parameters_is_stripped_too() {
        // Each of these turned up in the index as a second copy of a page that was
        // already there under its clean URL.
        let cases = [
            (
                "https://www.amazon.com/amazonprime?primeCampaignId=studentWlpPrimeRedir",
                "https://www.amazon.com/amazonprime",
            ),
            (
                "https://www.amazon.com/dp/B0?pf_rd_p=abc&pf_rd_r=def&th=1",
                "https://www.amazon.com/dp/B0?th=1",
            ),
            (
                "https://www.google.com/travel/flights/unsupported?ucbcb=1&ucpp=CjlodHR0",
                "https://www.google.com/travel/flights/unsupported",
            ),
            (
                "https://www.amazon.com/fmc/grocery-storefront?gsc=fe15Lls6uIyOG",
                "https://www.amazon.com/fmc/grocery-storefront",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(canonical_url(input).as_deref(), Some(expected), "{input}");
        }
    }

    #[test]
    fn the_fragment_is_dropped_and_the_default_port_folded() {
        assert_eq!(
            canonical_url("https://Example.COM:443/p#section").as_deref(),
            Some("https://example.com/p")
        );
    }

    #[test]
    fn unfetchable_urls_are_rejected_rather_than_canonicalised() {
        assert_eq!(canonical_url("mailto:someone@example.com"), None);
        assert_eq!(canonical_url("javascript:void(0)"), None);
        assert_eq!(canonical_url("ftp://example.com/f"), None);
        assert_eq!(canonical_url("not a url"), None);
        assert_eq!(canonical_url(""), None);
    }

    #[test]
    fn tracking_prefixes_are_recognised_case_insensitively() {
        assert!(is_tracking_param("UTM_Source"));
        assert!(is_tracking_param("utm_campaign"));
        assert!(is_tracking_param("pk_campaign"));
        assert!(is_tracking_param("fbclid"));
        assert!(!is_tracking_param("page"));
        assert!(!is_tracking_param("utm")); // the prefix has the underscore for a reason
        // Not a prefix match: a nearby name that merely starts the same way must
        // survive.
        assert!(!is_tracking_param("_gallery"));
    }

    #[test]
    fn obvious_non_pages_are_recognised_by_extension() {
        for url in [
            "https://example.com/logo.png",
            "https://example.com/app.js",
            "https://example.com/manual.pdf",
            "https://example.com/style.css",
            "https://example.com/archive.tar.gz",
            "https://example.com/font.woff2",
        ] {
            assert!(!is_probably_page(url), "{url} should not be fetched");
        }
    }

    #[test]
    fn pages_are_not_mistaken_for_assets() {
        for url in [
            "https://example.com/",
            "https://example.com/docs/index.html",
            "https://example.com/blog/2024/rust-is-fine",
            "https://example.com/sitemap.xml",
            "https://example.com/sitemap.xml.gz",
            "https://example.com/page.aspx?id=1",
            "https://example.com/.well-known/security.txt",
        ] {
            assert!(is_probably_page(url), "{url} should be fetched");
        }
    }

    #[test]
    fn a_gzipped_sitemap_is_still_a_sitemap() {
        assert!(is_sitemap_url("https://example.com/sitemap.xml.gz"));
        assert!(is_sitemap_url("https://example.com/sitemap.xml"));
        assert!(is_sitemap_url("https://example.com/news/feed.xml"));
        assert!(!is_sitemap_url("https://example.com/docs"));
    }

    #[test]
    fn xml_is_told_apart_from_xhtml() {
        assert!(is_xml_content_type(Some("application/xml")));
        assert!(is_xml_content_type(Some("text/xml; charset=utf-8")));
        // xhtml is HTML, and treating it as a sitemap would throw the page away.
        assert!(!is_xml_content_type(Some("application/xhtml+xml")));
        assert!(!is_xml_content_type(Some("text/html")));
        assert!(!is_xml_content_type(None));
    }

    #[test]
    fn an_unrestricted_scope_allows_everything() {
        let scope = CrawlScope::unrestricted();
        assert!(!scope.is_restricted());
        assert!(scope.allows("https://anything.example/x"));
    }

    #[test]
    fn a_restricted_scope_keeps_the_crawl_on_its_seeds() {
        let scope = CrawlScope::new(
            true,
            &["https://github.com/".to_string()],
            &["docs.github.com".to_string()],
        );

        assert!(scope.is_restricted());
        assert!(scope.allows("https://github.com/features"));
        assert!(scope.allows("https://docs.github.com/en"));
        // The whole point: another site that merely mentions github is not part
        // of a crawl of github.
        assert!(!scope.allows("https://news.example/github-raises"));
        // A subdomain is a different site until it is named.
        assert!(!scope.allows("https://gist.github.com/"));
    }

    #[test]
    fn naming_an_allowed_host_restricts_without_the_flag() {
        let scope = CrawlScope::new(false, &[], &["example.com".to_string()]);
        assert!(scope.is_restricted());
        assert!(scope.allows("https://example.com/a"));
        assert!(!scope.allows("https://other.example/a"));
    }

    #[test]
    fn the_hosts_in_scope_are_reported_in_a_stable_order() {
        let scope = CrawlScope::new(
            true,
            &[
                "https://github.com/".to_string(),
                "https://www.amazon.com/".to_string(),
            ],
            &["HELP.Example.com".to_string()],
        );
        assert_eq!(
            scope.hosts(),
            vec!["github.com", "help.example.com", "www.amazon.com"]
        );
    }

    #[test]
    fn a_host_with_a_port_and_a_case_difference_is_one_host() {
        let scope = CrawlScope::new(true, &["https://Example.COM:8443/".to_string()], &[]);
        assert!(scope.allows("https://example.com:8443/page"));
    }
}
