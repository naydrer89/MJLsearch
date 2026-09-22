//! Streaming HTML extraction.
//!
//! `lol_html` is a *rewriter*, not an extractor: handlers are invoked as tags
//! stream past and text is accumulated into buffers, so the full tree is never
//! materialised. That matters because this is the step that would otherwise turn
//! a 5 MB page into 50 MB of heap.
//!
//! Crucially the rewriter is driven with an output sink that *discards*
//! everything. The convenient `rewrite_str` entry point would serialise the whole
//! rewritten document into a `String`, which would double the memory cost of
//! every page for output nobody reads.
//!
//! Boilerplate removal is structural rather than a readability model: a depth
//! counter is raised while inside `nav`, `header`, `footer`, `aside`, `script`,
//! `style`, `noscript`, `template`, `svg`, `form` or `iframe`, and text captured
//! only at depth zero. A readability model would need the whole document in
//! memory, which is the thing this design is avoiding.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use lol_html::{HtmlRewriter, OutputSink, Settings, element, end_tag, text};

use crate::CrawlError;

/// Elements whose content is chrome rather than content.
const BOILERPLATE_SELECTOR: &str =
    "nav, header, footer, aside, script, style, noscript, template, svg, form, iframe";

/// Everything extracted from one page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedPage {
    /// Contents of `<title>`, trimmed, falling back to the page URL.
    pub title: String,
    /// Visible main text, with boilerplate removed and whitespace collapsed.
    pub text: String,
    /// Absolute, deduplicated outlinks.
    pub links: Vec<String>,
    /// Contents of `<meta name="description">`, falling back to `og:description`.
    pub meta_description: Option<String>,
}

/// Discards rewritten output, so no document copy is ever materialised.
struct NullSink;

impl OutputSink for NullSink {
    fn handle_chunk(&mut self, _chunk: &[u8]) {}
}

/// Extracts title, text, links and metadata from HTML.
///
/// `base_url` resolves relative links and is the fallback title for pages that
/// have none, which in practice is a large minority of the web.
pub fn parse(html: &str, base_url: &str) -> Result<ParsedPage, CrawlError> {
    let mut title = String::new();
    let mut body_text = String::new();
    let mut links: Vec<String> = Vec::new();
    let mut description = None;
    let mut open_graph_description = None;
    let mut base_href = None;

    let skip_depth = Rc::new(Cell::new(0usize));
    // Only the first `<title>` is the document title. Later ones are SVG titles
    // inside body images: github.com's homepage carries a customer-logo marquee
    // whose every `logo.svg` has one, and collecting them turned the title into
    // a run of company names after the real one.
    let title_taken = Rc::new(Cell::new(false));

    {
        let title_taken_flag = Rc::clone(&title_taken);
        let title_out = &mut title;
        let text_out = &mut body_text;
        let links_out = &mut links;
        let description_out = &mut description;
        let open_graph_out = &mut open_graph_description;
        let base_out = &mut base_href;

        let skip_on_enter = Rc::clone(&skip_depth);
        let skip_on_leave = Rc::clone(&skip_depth);
        let skip_for_text = Rc::clone(&skip_depth);

        // lol_html 3.x builds settings through a chain rather than a struct
        // literal: the fields are private, and each handler pair is appended.
        let settings = Settings::new()
            .append_element_content_handler(text!("title", move |chunk| {
                if title_taken_flag.get() {
                    return Ok(());
                }
                title_taken_flag.set(true);
                push_decoded(title_out, chunk.as_str());
                Ok(())
            }))
            // Raising a counter rather than a boolean, so nested boilerplate
            // (a `nav` inside a `header`) unwinds correctly.
            .append_element_content_handler(element!(BOILERPLATE_SELECTOR, move |el| {
                skip_on_enter.set(skip_on_enter.get() + 1);
                let counter = Rc::clone(&skip_on_leave);
                el.on_end_tag(end_tag!(move |_| {
                    counter.set(counter.get().saturating_sub(1));
                    Ok(())
                }))?;
                Ok(())
            }))
            .append_element_content_handler(text!("body", move |chunk| {
                if skip_for_text.get() == 0 {
                    push_decoded(text_out, chunk.as_str());
                    text_out.push(' ');
                }
                Ok(())
            }))
            .append_element_content_handler(element!("a[href]", move |el| {
                if let Some(href) = el.get_attribute("href") {
                    links_out.push(decode_entities(&href));
                }
                Ok(())
            }))
            .append_element_content_handler(element!("base[href]", move |el| {
                if let Some(href) = el.get_attribute("href") {
                    *base_out = Some(href);
                }
                Ok(())
            }))
            .append_element_content_handler(element!("meta[name=description]", move |el| {
                if let Some(content) = el.get_attribute("content") {
                    *description_out = Some(decode_entities(&content));
                }
                Ok(())
            }))
            .append_element_content_handler(element!(
                "meta[property='og:description']",
                move |el| {
                    if let Some(content) = el.get_attribute("content") {
                        *open_graph_out = Some(decode_entities(&content));
                    }
                    Ok(())
                }
            ));

        let mut rewriter = HtmlRewriter::new(settings, NullSink);
        rewriter
            .write(html.as_bytes())
            .map_err(|error| CrawlError::Parse(error.to_string()))?;
        rewriter
            .end()
            .map_err(|error| CrawlError::Parse(error.to_string()))?;
    }

    let title = collapse_whitespace(&title);
    let title = if title.is_empty() {
        base_url.to_string()
    } else {
        title
    };

    let resolution_base = base_href.as_deref().unwrap_or(base_url);

    Ok(ParsedPage {
        title,
        text: collapse_whitespace(&body_text),
        links: resolve_links(&links, resolution_base),
        meta_description: description
            .or(open_graph_description)
            .map(|value| collapse_whitespace(&value))
            .filter(|value| !value.is_empty()),
    })
}

/// Decodes the HTML character references that appear in attribute values.
///
/// `lol_html` hands back attribute values as they were written in the source, so
/// a link to `/search?x=1&amp;y=2` would otherwise be enqueued literally, with a
/// literal `&amp;` in the query string, and no server would recognise it. This
/// is common enough on real pages that skipping it silently breaks a share of
/// every crawl.
///
/// Only the references that realistically appear in URLs and descriptions are
/// decoded; anything unrecognised is left as written rather than dropped.
pub fn decode_entities(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    push_decoded(&mut output, input);
    output
}

/// Appends `input` to `output`, decoding character references on the way.
///
/// A buffer-taking form so whole-document text can be decoded chunk by chunk
/// without allocating a `String` per text node — a real page has thousands of
/// them, and the title and body paths both need this.
pub fn push_decoded(output: &mut String, input: &str) {
    if !input.contains('&') {
        output.push_str(input);
        return;
    }

    let mut rest = input;

    while let Some(index) = rest.find('&') {
        output.push_str(&rest[..index]);
        rest = &rest[index..];

        match decode_reference(rest) {
            Some((character, consumed)) => {
                output.push(character);
                rest = &rest[consumed..];
            }
            None => {
                // A bare `&` is legal in HTML text and attribute values.
                output.push('&');
                rest = &rest[1..];
            }
        }
    }

    output.push_str(rest);
}

/// Decodes one reference at the start of `input`, which must begin with `&`.
fn decode_reference(input: &str) -> Option<(char, usize)> {
    let semicolon = input.find(';')?;
    let entity = &input[1..semicolon];
    // Guards against scanning a long stretch of text for a `;` that never comes.
    if entity.is_empty() || entity.len() > 12 {
        return None;
    }
    let consumed = semicolon + 1;

    let character = match entity {
        // The five XML entities, which is the set a naive decoder stops at. It is not
        // the set a web page uses: `&mdash;` in a title is ordinary prose, and a
        // reference that is not decoded arrives in a search result verbatim -- "Logo
        // &mdash; Blender" is what the top result for Blender used to read.
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',

        // Spaces and dashes: the two families that actually show up in titles.
        "nbsp" => '\u{a0}',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "shy" => '\u{ad}',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "horbar" => '\u{2015}',
        "minus" => '\u{2212}',

        // Quotation and punctuation.
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "sbquo" => '\u{201a}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "bdquo" => '\u{201e}',
        "laquo" => '\u{ab}',
        "raquo" => '\u{bb}',
        "lsaquo" => '\u{2039}',
        "rsaquo" => '\u{203a}',
        "hellip" => '\u{2026}',
        "bull" => '\u{2022}',
        "middot" => '\u{b7}',
        "dagger" => '\u{2020}',
        "Dagger" => '\u{2021}',
        "prime" => '\u{2032}',
        "Prime" => '\u{2033}',
        "permil" => '\u{2030}',
        "oline" => '\u{203e}',
        "frasl" => '\u{2044}',

        // Signs, currency and legal marks.
        "iexcl" => '\u{a1}',
        "iquest" => '\u{bf}',
        "sect" => '\u{a7}',
        "para" => '\u{b6}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        "deg" => '\u{b0}',
        "micro" => '\u{b5}',
        "plusmn" => '\u{b1}',
        "times" => '\u{d7}',
        "divide" => '\u{f7}',
        "not" => '\u{ac}',
        "cent" => '\u{a2}',
        "pound" => '\u{a3}',
        "yen" => '\u{a5}',
        "euro" => '\u{20ac}',
        "curren" => '\u{a4}',
        "brvbar" => '\u{a6}',
        "uml" => '\u{a8}',
        "acute" => '\u{b4}',
        "cedil" => '\u{b8}',
        "macr" => '\u{af}',
        "circ" => '\u{2c6}',
        "tilde" => '\u{2dc}',
        "sup1" => '\u{b9}',
        "sup2" => '\u{b2}',
        "sup3" => '\u{b3}',
        "frac14" => '\u{bc}',
        "frac12" => '\u{bd}',
        "frac34" => '\u{be}',
        "ordf" => '\u{aa}',
        "ordm" => '\u{ba}',

        // Accented letters: a European page's title is full of them.
        "Agrave" => '\u{c0}',
        "Aacute" => '\u{c1}',
        "Acirc" => '\u{c2}',
        "Atilde" => '\u{c3}',
        "Auml" => '\u{c4}',
        "Aring" => '\u{c5}',
        "AElig" => '\u{c6}',
        "Ccedil" => '\u{c7}',
        "Egrave" => '\u{c8}',
        "Eacute" => '\u{c9}',
        "Ecirc" => '\u{ca}',
        "Euml" => '\u{cb}',
        "Igrave" => '\u{cc}',
        "Iacute" => '\u{cd}',
        "Icirc" => '\u{ce}',
        "Iuml" => '\u{cf}',
        "ETH" => '\u{d0}',
        "Ntilde" => '\u{d1}',
        "Ograve" => '\u{d2}',
        "Oacute" => '\u{d3}',
        "Ocirc" => '\u{d4}',
        "Otilde" => '\u{d5}',
        "Ouml" => '\u{d6}',
        "Oslash" => '\u{d8}',
        "Ugrave" => '\u{d9}',
        "Uacute" => '\u{da}',
        "Ucirc" => '\u{db}',
        "Uuml" => '\u{dc}',
        "Yacute" => '\u{dd}',
        "THORN" => '\u{de}',
        "szlig" => '\u{df}',
        "agrave" => '\u{e0}',
        "aacute" => '\u{e1}',
        "acirc" => '\u{e2}',
        "atilde" => '\u{e3}',
        "auml" => '\u{e4}',
        "aring" => '\u{e5}',
        "aelig" => '\u{e6}',
        "ccedil" => '\u{e7}',
        "egrave" => '\u{e8}',
        "eacute" => '\u{e9}',
        "ecirc" => '\u{ea}',
        "euml" => '\u{eb}',
        "igrave" => '\u{ec}',
        "iacute" => '\u{ed}',
        "icirc" => '\u{ee}',
        "iuml" => '\u{ef}',
        "eth" => '\u{f0}',
        "ntilde" => '\u{f1}',
        "ograve" => '\u{f2}',
        "oacute" => '\u{f3}',
        "ocirc" => '\u{f4}',
        "otilde" => '\u{f5}',
        "ouml" => '\u{f6}',
        "oslash" => '\u{f8}',
        "ugrave" => '\u{f9}',
        "uacute" => '\u{fa}',
        "ucirc" => '\u{fb}',
        "uuml" => '\u{fc}',
        "yacute" => '\u{fd}',
        "thorn" => '\u{fe}',
        "yuml" => '\u{ff}',
        "OElig" => '\u{152}',
        "oelig" => '\u{153}',
        "Scaron" => '\u{160}',
        "scaron" => '\u{161}',
        "Yuml" => '\u{178}',
        "fnof" => '\u{192}',

        // Arrows and maths, which appear in documentation and changelogs.
        "larr" => '\u{2190}',
        "uarr" => '\u{2191}',
        "rarr" => '\u{2192}',
        "darr" => '\u{2193}',
        "harr" => '\u{2194}',
        "crarr" => '\u{21b5}',
        "forall" => '\u{2200}',
        "part" => '\u{2202}',
        "exist" => '\u{2203}',
        "empty" => '\u{2205}',
        "nabla" => '\u{2207}',
        "isin" => '\u{2208}',
        "notin" => '\u{2209}',
        "ni" => '\u{220b}',
        "prod" => '\u{220f}',
        "sum" => '\u{2211}',
        "lowast" => '\u{2217}',
        "radic" => '\u{221a}',
        "prop" => '\u{221d}',
        "infin" => '\u{221e}',
        "ang" => '\u{2220}',
        "and" => '\u{2227}',
        "or" => '\u{2228}',
        "cap" => '\u{2229}',
        "cup" => '\u{222a}',
        "int" => '\u{222b}',
        "there4" => '\u{2234}',
        "sim" => '\u{223c}',
        "cong" => '\u{2245}',
        "asymp" => '\u{2248}',
        "ne" => '\u{2260}',
        "equiv" => '\u{2261}',
        "le" => '\u{2264}',
        "ge" => '\u{2265}',
        "sub" => '\u{2282}',
        "sup" => '\u{2283}',
        "nsub" => '\u{2284}',
        "sube" => '\u{2286}',
        "supe" => '\u{2287}',
        "oplus" => '\u{2295}',
        "otimes" => '\u{2297}',
        "perp" => '\u{22a5}',
        "sdot" => '\u{22c5}',
        "lceil" => '\u{2308}',
        "rceil" => '\u{2309}',
        "lfloor" => '\u{230a}',
        "rfloor" => '\u{230b}',
        "lang" => '\u{27e8}',
        "rang" => '\u{27e9}',
        "loz" => '\u{25ca}',

        // Greek, for the parts of the web that write about maths and physics.
        "Alpha" => '\u{391}',
        "Beta" => '\u{392}',
        "Gamma" => '\u{393}',
        "Delta" => '\u{394}',
        "Epsilon" => '\u{395}',
        "Zeta" => '\u{396}',
        "Eta" => '\u{397}',
        "Theta" => '\u{398}',
        "Iota" => '\u{399}',
        "Kappa" => '\u{39a}',
        "Lambda" => '\u{39b}',
        "Mu" => '\u{39c}',
        "Nu" => '\u{39d}',
        "Xi" => '\u{39e}',
        "Omicron" => '\u{39f}',
        "Pi" => '\u{3a0}',
        "Rho" => '\u{3a1}',
        "Sigma" => '\u{3a3}',
        "Tau" => '\u{3a4}',
        "Upsilon" => '\u{3a5}',
        "Phi" => '\u{3a6}',
        "Chi" => '\u{3a7}',
        "Psi" => '\u{3a8}',
        "Omega" => '\u{3a9}',
        "alpha" => '\u{3b1}',
        "beta" => '\u{3b2}',
        "gamma" => '\u{3b3}',
        "delta" => '\u{3b4}',
        "epsilon" => '\u{3b5}',
        "zeta" => '\u{3b6}',
        "eta" => '\u{3b7}',
        "theta" => '\u{3b8}',
        "iota" => '\u{3b9}',
        "kappa" => '\u{3ba}',
        "lambda" => '\u{3bb}',
        "mu" => '\u{3bc}',
        "nu" => '\u{3bd}',
        "xi" => '\u{3be}',
        "omicron" => '\u{3bf}',
        "pi" => '\u{3c0}',
        "rho" => '\u{3c1}',
        "sigmaf" => '\u{3c2}',
        "sigma" => '\u{3c3}',
        "tau" => '\u{3c4}',
        "upsilon" => '\u{3c5}',
        "phi" => '\u{3c6}',
        "chi" => '\u{3c7}',
        "psi" => '\u{3c8}',
        "omega" => '\u{3c9}',

        // Card suits, which is not a joke: they are on every casino and games page.
        "spades" => '\u{2660}',
        "clubs" => '\u{2663}',
        "hearts" => '\u{2665}',
        "diams" => '\u{2666}',

        _ => {
            let digits = entity.strip_prefix('#')?;
            match digits.strip_prefix(['x', 'X']) {
                Some(hex) => char::from_u32(u32::from_str_radix(hex, 16).ok()?)?,
                None => char::from_u32(digits.parse().ok()?)?,
            }
        }
    };

    Some((character, consumed))
}

/// Collapses runs of whitespace into single spaces and trims the ends.
///
/// HTML source is full of indentation that is not present when rendered, and
/// leaving it in place would both bloat the index and make snippets read badly.
pub fn collapse_whitespace(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut pending_space = false;

    for character in input.chars() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(character);
    }

    output
}

/// Resolves raw `href` values against a base, keeping only fetchable, unique URLs.
///
/// Fragments are stripped because `/page` and `/page#section` are the same
/// document, and indexing both would duplicate every page that uses anchors.
pub fn resolve_links(raw: &[String], base: &str) -> Vec<String> {
    let Ok(base) = url::Url::parse(base) else {
        return Vec::new();
    };

    // Bounded by the number of links on a single page, not by the crawl, so a set
    // here is not the unbounded-growth problem that a set of all seen URLs is.
    let mut seen: HashSet<String> = HashSet::new();
    let mut resolved_links = Vec::new();

    for candidate in raw {
        let candidate = candidate.trim();
        if candidate.is_empty() || candidate.starts_with('#') {
            continue;
        }

        let Ok(mut resolved) = base.join(candidate) else {
            continue;
        };
        if !matches!(resolved.scheme(), "http" | "https") {
            continue;
        }
        resolved.set_fragment(None);

        let normalized = resolved.to_string();
        if seen.insert(normalized.clone()) {
            resolved_links.push(normalized);
        }
    }

    resolved_links
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://example.com/dir/page.html";

    #[test]
    fn extracts_an_ordinary_page() {
        let page = parse(
            "<html><head><title> Hello World </title></head>\
             <body><h1>Heading</h1><p>Some body text.</p></body></html>",
            BASE,
        )
        .expect("parse");

        assert_eq!(page.title, "Hello World");
        assert!(page.text.contains("Heading"));
        assert!(page.text.contains("Some body text."));
    }

    #[test]
    fn a_page_without_a_title_falls_back_to_its_url() {
        let page = parse("<html><body>text</body></html>", BASE).expect("parse");
        assert_eq!(page.title, BASE);
    }

    #[test]
    fn boilerplate_is_excluded_from_the_text() {
        let page = parse(
            "<html><body>\
             <nav>Home About Contact</nav>\
             <header>Site header</header>\
             <main>Real content here</main>\
             <footer>Copyright notice</footer>\
             </body></html>",
            BASE,
        )
        .expect("parse");

        assert!(page.text.contains("Real content here"));
        for chrome in ["Home About Contact", "Site header", "Copyright notice"] {
            assert!(
                !page.text.contains(chrome),
                "{chrome:?} should have been dropped"
            );
        }
    }

    #[test]
    fn script_and_style_contents_never_reach_the_index() {
        let page = parse(
            "<html><body><script>var secret = 1;</script>\
             <style>.a { color: red }</style>\
             <p>visible</p></body></html>",
            BASE,
        )
        .expect("parse");

        assert!(page.text.contains("visible"));
        assert!(!page.text.contains("secret"));
        assert!(!page.text.contains("color: red"));
    }

    #[test]
    fn nested_boilerplate_unwinds_correctly() {
        // A boolean flag instead of a counter would leave the crawler convinced it
        // was still inside chrome and silently drop everything after this.
        let page = parse(
            "<html><body><header><nav><span>deep chrome</span></nav></header>\
             <p>after the chrome</p></body></html>",
            BASE,
        )
        .expect("parse");

        assert!(page.text.contains("after the chrome"));
        assert!(!page.text.contains("deep chrome"));
    }

    #[test]
    fn a_relative_link_is_resolved_against_the_page() {
        let page = parse(r#"<body><a href="other.html">x</a></body>"#, BASE).expect("parse");
        assert_eq!(
            page.links,
            vec!["https://example.com/dir/other.html".to_string()]
        );
    }

    #[test]
    fn an_absolute_and_a_protocol_relative_link_both_resolve() {
        let page = parse(
            r#"<body><a href="https://other.example/a">1</a><a href="//cdn.example/b">2</a></body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(
            page.links,
            vec![
                "https://other.example/a".to_string(),
                "https://cdn.example/b".to_string()
            ]
        );
    }

    #[test]
    fn a_base_element_overrides_the_page_url() {
        let page = parse(
            r#"<head><base href="https://cdn.example/assets/"></head>
               <body><a href="thing.html">x</a></body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(
            page.links,
            vec!["https://cdn.example/assets/thing.html".to_string()]
        );
    }

    #[test]
    fn fragments_are_stripped_so_one_document_is_not_indexed_twice() {
        let page = parse(
            r#"<body><a href="/page#intro">1</a><a href="/page#outro">2</a></body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(page.links, vec!["https://example.com/page".to_string()]);
    }

    #[test]
    fn duplicate_links_are_collapsed() {
        let page = parse(
            r#"<body><a href="/a">1</a><a href="/a">2</a><a href="https://example.com/a">3</a></body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(page.links, vec!["https://example.com/a".to_string()]);
    }

    #[test]
    fn a_dot_relative_link_is_resolved_from_the_current_directory() {
        // Worth pinning: this is a different URL from the root-relative `/a`, and
        // treating the two as duplicates would lose a page.
        let page = parse(
            r#"<body><a href="/a">1</a><a href="./a">2</a></body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(
            page.links,
            vec![
                "https://example.com/a".to_string(),
                "https://example.com/dir/a".to_string()
            ]
        );
    }

    #[test]
    fn non_fetchable_schemes_are_dropped() {
        let page = parse(
            r#"<body>
               <a href="mailto:someone@example.com">mail</a>
               <a href="javascript:void(0)">js</a>
               <a href="tel:+123">phone</a>
               <a href="data:text/plain,hi">data</a>
               <a href="/keep">keep</a>
               </body>"#,
            BASE,
        )
        .expect("parse");

        // Enqueuing any of these would poison the frontier with URLs that can
        // never be fetched.
        assert_eq!(page.links, vec!["https://example.com/keep".to_string()]);
    }

    #[test]
    fn an_empty_href_is_ignored() {
        let page =
            parse(r#"<body><a href="">x</a><a href="   ">y</a></body>"#, BASE).expect("parse");
        assert!(page.links.is_empty());
    }

    #[test]
    fn entities_in_attribute_values_are_decoded() {
        let page =
            parse(r#"<body><a href="/search?x=1&amp;y=2">q</a></body>"#, BASE).expect("parse");

        assert_eq!(
            page.links,
            vec!["https://example.com/search?x=1&y=2".to_string()]
        );
    }

    #[test]
    fn entities_in_text_and_titles_are_decoded() {
        // The snippet is built from the body text, so a reference left here is
        // read by the searcher: github.com ships `GitHub&#x27;s database`.
        let page = parse(
            r#"<head><title>Mad &amp; Co&#x27;s &quot;shop&quot;</title></head>
               <body><p>GitHub&#x27;s database of &lt;known&gt; issues</p></body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(page.title, "Mad & Co's \"shop\"");
        assert_eq!(page.text, "GitHub's database of <known> issues");
    }

    #[test]
    fn only_the_first_title_counts() {
        // github.com's homepage puts a `<title>` in every logo SVG of its
        // customer marquee; collecting them appended a run of company names to
        // the real title.
        let page = parse(
            r#"<head><title>GitHub &#183; Change is constant.</title></head>
               <body><svg><title>American Airlines</title></svg>
               <svg><title>Duolingo</title></svg></body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(page.title, "GitHub \u{b7} Change is constant.");
    }

    #[test]
    fn entity_decoding_covers_the_realistic_cases() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_entities("&quot;quoted&quot;"), "\"quoted\"");
        assert_eq!(decode_entities("it&#39;s"), "it's");
        assert_eq!(decode_entities("it&apos;s"), "it's");
        // Numeric and hex references, which sites use for non-ASCII paths.
        assert_eq!(decode_entities("caf&#233;"), "caf\u{e9}");
        assert_eq!(decode_entities("caf&#xe9;"), "caf\u{e9}");
    }

    #[test]
    fn named_references_from_real_pages_are_decoded() {
        // The five XML entities are not the set a web page uses. Each of these was read
        // off a title that the index actually held: `&mdash;` from blender.org,
        // `&hellip;` from a docs page, `&eacute;` from a European vendor site.
        assert_eq!(
            decode_entities("Logo &mdash; Blender"),
            "Logo \u{2014} Blender"
        );
        assert_eq!(decode_entities("a&ndash;b"), "a\u{2013}b");
        assert_eq!(decode_entities("wait&hellip;"), "wait\u{2026}");
        assert_eq!(
            decode_entities("&laquo;quoted&raquo;"),
            "\u{ab}quoted\u{bb}"
        );
        assert_eq!(decode_entities("&lsquo;x&rsquo;"), "\u{2018}x\u{2019}");
        assert_eq!(decode_entities("caf&eacute;"), "caf\u{e9}");
        assert_eq!(decode_entities("&uuml;ber"), "\u{fc}ber");
        assert_eq!(decode_entities("na&iuml;ve"), "na\u{ef}ve");
        assert_eq!(
            decode_entities("&copy;&reg;&trade;"),
            "\u{a9}\u{ae}\u{2122}"
        );
        assert_eq!(decode_entities("&euro;19"), "\u{20ac}19");
        assert_eq!(
            decode_entities("&pi; &asymp; 3.14"),
            "\u{3c0} \u{2248} 3.14"
        );
        assert_eq!(decode_entities("a&nbsp;b"), "a\u{a0}b");
        assert_eq!(decode_entities("x&rarr;y"), "x\u{2192}y");
        // And the two forms still mix, which is the common case in a real title.
        assert_eq!(decode_entities("A &amp; B &#8212; C"), "A & B \u{2014} C");
    }

    #[test]
    fn entity_decoding_leaves_what_it_does_not_understand_alone() {
        // Dropping an unrecognised reference would silently corrupt a URL.
        assert_eq!(decode_entities("&unknownentity;"), "&unknownentity;");
        assert_eq!(decode_entities("100% & rising"), "100% & rising");
        assert_eq!(decode_entities("&"), "&");
        assert_eq!(decode_entities(""), "");
        assert_eq!(decode_entities("plain"), "plain");
        // A very long run without a semicolon must not be scanned forever.
        let hostile = format!("&{}", "9".repeat(200));
        assert_eq!(decode_entities(&hostile), hostile);
    }

    #[test]
    fn meta_description_is_read() {
        let page = parse(
            r#"<head><meta name="description" content="A description."></head><body>x</body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(page.meta_description.as_deref(), Some("A description."));
    }

    #[test]
    fn the_named_description_beats_the_open_graph_one() {
        let page = parse(
            r#"<head>
               <meta property="og:description" content="OG text">
               <meta name="description" content="Preferred text">
               </head><body>x</body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(page.meta_description.as_deref(), Some("Preferred text"));
    }

    #[test]
    fn the_open_graph_description_is_used_as_a_fallback() {
        let page = parse(
            r#"<head><meta property="og:description" content="OG text"></head><body>x</body>"#,
            BASE,
        )
        .expect("parse");

        assert_eq!(page.meta_description.as_deref(), Some("OG text"));
    }

    #[test]
    fn whitespace_is_collapsed() {
        let page = parse(
            "<html><body><p>one\n\n   two\t\tthree</p></body></html>",
            BASE,
        )
        .expect("parse");

        assert!(page.text.contains("one two three"), "got {:?}", page.text);
        assert!(!page.text.contains("  "));
    }

    #[test]
    fn collapse_whitespace_handles_the_edges() {
        assert_eq!(collapse_whitespace(""), "");
        assert_eq!(collapse_whitespace("   "), "");
        assert_eq!(collapse_whitespace("  a  b  "), "a b");
        assert_eq!(collapse_whitespace("a\nb"), "a b");
        // A non-breaking space is Unicode whitespace, so it collapses like any
        // other. That is the behaviour we want: it renders as a space, and
        // leaving it in would split otherwise-identical terms in the index.
        assert_eq!(collapse_whitespace("a\u{a0}b"), "a b");
    }

    #[test]
    fn unclosed_tags_do_not_break_extraction() {
        // Real pages are full of this, and a parser that bailed out here would
        // lose most of its input.
        let page = parse(
            "<html><body><div><p>unclosed paragraph<p>another<nav>chrome</body>",
            BASE,
        )
        .expect("parse");

        assert!(page.text.contains("unclosed paragraph"));
        assert!(page.text.contains("another"));
    }

    #[test]
    fn an_empty_document_produces_an_empty_page() {
        let page = parse("", BASE).expect("parse");
        assert_eq!(page.title, BASE);
        assert!(page.text.is_empty());
        assert!(page.links.is_empty());
        assert_eq!(page.meta_description, None);
    }

    #[test]
    fn a_document_that_is_not_html_at_all_still_parses() {
        let page = parse("just some plain text", BASE).expect("parse");
        assert!(page.title == BASE);
    }

    #[test]
    fn deeply_nested_markup_is_handled() {
        let mut html = String::from("<html><body>");
        for _ in 0..500 {
            html.push_str("<div>");
        }
        html.push_str("bottom");
        for _ in 0..500 {
            html.push_str("</div>");
        }
        html.push_str("</body></html>");

        let page = parse(&html, BASE).expect("parse");
        assert!(page.text.contains("bottom"));
    }

    #[test]
    fn an_unparsable_base_url_yields_no_links_rather_than_an_error() {
        let page = parse(r#"<body><a href="/a">x</a></body>"#, "not a url").expect("parse");
        assert!(page.links.is_empty());
    }

    #[test]
    fn text_outside_the_body_element_is_not_indexed() {
        let page = parse(
            "<html><head><title>T</title><meta name=\"x\" content=\"y\"></head><body>only this</body></html>",
            BASE,
        )
        .expect("parse");

        assert_eq!(page.title, "T");
        assert!(page.text.contains("only this"));
    }
}
