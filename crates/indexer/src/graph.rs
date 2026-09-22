//! Link-graph authority: which pages the crawl's own link structure says matter.
//!
//! ## Why this exists
//!
//! Text relevance alone cannot tell the difference between a company's home page
//! and its privacy policy: both contain the company's name, and the policy often
//! contains it more often. What separates them is that the site itself links to
//! the home page from everywhere and to the policy only from a footer — which is
//! exactly what a link graph measures, and it is how every search engine of
//! consequence made this judgement before neural models existed.
//!
//! ## The two scores
//!
//! * **Page authority** is PageRank over the documents in one drain, with links
//!   resolved the way the crawler resolved them. It answers "is this the page
//!   about the thing, or a page that merely mentions the thing".
//! * **Host authority** counts links that arrive from *other* sites, and is
//!   accumulated across runs on disk. It answers "does anyone else consider this
//!   site worth linking to", which a single batch of one site's own pages can
//!   never say.
//!
//! ## Why the scores are relative rather than raw
//!
//! Raw PageRank is not comparable between runs: its scale depends on how many
//! documents were in the batch that produced it, so a page indexed in a small
//! drain would look important next to one indexed in a large drain. Both scores
//! are therefore expressed against the *average* page — `n * rank` is 1 for an
//! average page and does not change when the corpus grows — and then squashed
//! into `[0, 1]` with a saturating exponential, which has no cliff to fall off.
//!
//! ## The bound
//!
//! The graph is held in memory for the duration of a drain, because a rank
//! iteration over a graph that lives on disk is a different and much larger
//! project. It is capped by [`GraphLimits`], and a drain that hits the cap says
//! so in its log rather than silently reporting authority for half a corpus.

use std::collections::{BTreeMap, HashMap, HashSet};

use search_core::DocScores;

/// How many times the rank is propagated across the graph.
///
/// Twenty is well past the point where the ordering stops changing on graphs of
/// this shape, and each iteration is a single pass over the edge list.
const ITERATIONS: usize = 20;

/// The classic damping factor: a fifth of the importance is spread over the whole
/// graph, which is what keeps a page with a single, unrelated inlink from
/// inheriting all of it.
const DAMPING: f32 = 0.85;

/// `n * rank` at which the squashed page score reaches `1 - 1/e`.
const AUTHORITY_SCALE: f32 = 4.0;

/// Cross-site inlinks at which the squashed host score reaches `1 - 1/e`.
const HOST_AUTHORITY_SCALE: f32 = 8.0;

/// Bounds on what the graph will hold in memory.
#[derive(Debug, Clone, Copy)]
pub struct GraphLimits {
    /// Maximum distinct URLs tracked as nodes.
    pub max_nodes: usize,
    /// Maximum distinct links recorded as edges.
    pub max_edges: usize,
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            // ~250k nodes and two million edges is around a hundred megabytes of
            // adjacency at this representation, which is the point at which a
            // 64 MiB writer heap is no longer the biggest allocation in the process.
            max_nodes: 250_000,
            max_edges: 2_000_000,
        }
    }
}

/// The link graph of one drain, plus the persisted cross-site counts.
#[derive(Debug)]
pub struct LinkGraph {
    limits: GraphLimits,
    ids: HashMap<String, u32>,
    urls: Vec<String>,
    out: Vec<Vec<u32>>,
    edges: usize,
    truncated: bool,
    /// Host -> inlinks from other hosts. Carried in from disk and written back out.
    host_inlinks: BTreeMap<String, u64>,
}

impl LinkGraph {
    /// Builds an empty graph, seeded with the cross-site counts from previous runs.
    pub fn new(limits: GraphLimits, host_inlinks: BTreeMap<String, u64>) -> Self {
        Self {
            limits,
            ids: HashMap::new(),
            urls: Vec::new(),
            out: Vec::new(),
            edges: 0,
            truncated: false,
            host_inlinks,
        }
    }

    /// Records one document's outlinks.
    ///
    /// Links to URLs that were never crawled are not dropped: a page that links
    /// into a site we have not visited still tells us the *source* is the kind of
    /// page that links out, and the target still accumulates cross-site credit.
    /// They simply have no adjacency to distribute rank along.
    pub fn record(&mut self, url: &str, links: &[String]) {
        let source = self.intern(url);
        // A page that links to the same target five times has said it once.
        let mut seen: HashSet<u32> = HashSet::new();
        let mut targets: Vec<u32> = Vec::new();

        for link in links {
            if self.edges >= self.limits.max_edges || self.urls.len() >= self.limits.max_nodes {
                self.truncated = true;
                break;
            }
            let target = self.intern(link);
            if target == source || !seen.insert(target) {
                continue;
            }
            targets.push(target);
            self.edges += 1;
        }

        // Recording the same page twice replaces its links rather than adding to
        // them, and the edge count has to follow, or `shape` would report a graph
        // larger than the one the rank iteration walks. The loop above has already
        // added this call's targets, so what is left is to remove the ones it
        // replaced.
        let replaced = self.out[source as usize].len();
        self.edges = self.edges.saturating_sub(replaced);
        self.out[source as usize] = targets;
    }

    /// Interns a URL, returning its node id.
    fn intern(&mut self, url: &str) -> u32 {
        if let Some(id) = self.ids.get(url) {
            return *id;
        }
        let id = self.urls.len() as u32;
        self.ids.insert(url.to_string(), id);
        self.urls.push(url.to_string());
        self.out.push(Vec::new());
        id
    }

    /// Whether the graph stopped recording because it hit its bounds.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Nodes and edges recorded.
    pub fn shape(&self) -> (usize, usize) {
        (self.urls.len(), self.edges)
    }

    /// The cross-site counts, including this run's, for persisting.
    pub fn host_inlinks(&self) -> &BTreeMap<String, u64> {
        &self.host_inlinks
    }

    /// Computes both scores for every node.
    pub fn finish(&mut self) -> HashMap<String, DocScores> {
        self.count_cross_site_links();
        let ranks = self.page_rank();
        let node_count = self.urls.len() as f32;

        let mut scores = HashMap::with_capacity(self.urls.len());
        for (index, url) in self.urls.iter().enumerate() {
            // `node_count * rank` is the share of the graph's importance this page
            // holds relative to an even split. One means "exactly average".
            let relative = node_count * ranks[index];
            let host = host_of(url);
            let cross_site = host
                .as_ref()
                .and_then(|host| self.host_inlinks.get(host))
                .copied()
                .unwrap_or(0) as f32;

            scores.insert(
                url.clone(),
                DocScores {
                    authority: squash(relative, AUTHORITY_SCALE),
                    host_authority: squash(cross_site, HOST_AUTHORITY_SCALE),
                },
            );
        }

        scores
    }

    /// Adds this run's cross-site links to the persisted counts.
    ///
    /// Only links that leave the source's host count. A site's internal links
    /// measure how its own navigation is arranged, not whether anyone else
    /// considers it worth citing, and mixing the two would make a large site look
    /// authoritative purely for being large.
    fn count_cross_site_links(&mut self) {
        let mut pending: BTreeMap<String, u64> = BTreeMap::new();

        for (source, targets) in self.out.iter().enumerate() {
            let Some(source_host) = host_of(&self.urls[source]) else {
                continue;
            };
            for target in targets {
                let Some(target_host) = host_of(&self.urls[*target as usize]) else {
                    continue;
                };
                if target_host == source_host {
                    continue;
                }
                *pending.entry(target_host).or_insert(0) += 1;
            }
        }

        for (host, count) in pending {
            *self.host_inlinks.entry(host).or_insert(0) += count;
        }
    }

    /// PageRank with the dangling mass redistributed uniformly.
    ///
    /// Redistributing rather than letting it vanish keeps the rank vector summing
    /// to one, which matters here because the scores are compared *relative to the
    /// average*: leaked mass would shrink every page's figure by an amount that
    /// depends on how much of the corpus happens to be link-less, and the score
    /// would then be a fact about the corpus rather than about the page.
    fn page_rank(&self) -> Vec<f32> {
        let n = self.urls.len();
        if n == 0 {
            return Vec::new();
        }
        let uniform = (1.0 - DAMPING) / n as f32;
        let mut rank = vec![1.0 / n as f32; n];

        for _ in 0..ITERATIONS {
            let mut next = vec![uniform; n];
            let mut dangling = 0.0f32;

            for (node, targets) in self.out.iter().enumerate() {
                if targets.is_empty() {
                    dangling += rank[node];
                    continue;
                }
                let share = DAMPING * rank[node] / targets.len() as f32;
                for target in targets {
                    next[*target as usize] += share;
                }
            }

            let spread = DAMPING * dangling / n as f32;
            for value in next.iter_mut() {
                *value += spread;
            }
            rank = next;
        }

        rank
    }
}

/// Squashes a non-negative count into `[0, 1)`, saturating smoothly.
fn squash(value: f32, scale: f32) -> f32 {
    if value <= 0.0 || scale <= 0.0 {
        return 0.0;
    }
    1.0 - (-value / scale).exp()
}

/// The lowercased host of a URL, or `None` when there is not one.
fn host_of(url: &str) -> Option<String> {
    let without_scheme = url.split("://").nth(1)?;
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Reads the persisted cross-site counts. A missing or unreadable file is empty,
/// not an error: the first run has no history, and a corrupt history is worth
/// less than a working indexer.
pub fn load_host_inlinks(path: &std::path::Path) -> BTreeMap<String, u64> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str::<BTreeMap<String, u64>>(&raw) {
        Ok(counts) => counts,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "discarding an unreadable host-authority file and starting from zero"
            );
            BTreeMap::new()
        }
    }
}

/// Writes the cross-site counts, atomically so a reader never sees half of them.
pub fn save_host_inlinks(
    path: &std::path::Path,
    counts: &BTreeMap<String, u64>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string(counts).unwrap_or_else(|_| "{}".to_string());
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> LinkGraph {
        LinkGraph::new(GraphLimits::default(), BTreeMap::new())
    }

    fn links(urls: &[&str]) -> Vec<String> {
        urls.iter().map(|url| url.to_string()).collect()
    }

    /// A site whose home page every other page links to, and whose privacy policy
    /// only the home page links to.
    ///
    /// Worth being precise about what this does and does not prove: internal links
    /// separate a page the site points at from one it buries, which is the case
    /// here and the case on most real sites. On a site that puts its privacy link
    /// in the footer of *every* page, internal linking alone would rank the policy
    /// highly, and separating those two is the job of the ranking policy's URL
    /// heuristics rather than of this graph — which is exactly why the policy has
    /// them.
    fn site() -> LinkGraph {
        let mut graph = graph();
        let home = "https://example.com/";

        for path in ["about", "features", "pricing", "docs", "blog"] {
            graph.record(&format!("https://example.com/{path}"), &links(&[home]));
        }
        graph.record(
            home,
            &links(&["https://example.com/about", "https://example.com/privacy"]),
        );
        graph.record("https://example.com/privacy", &links(&[]));
        graph
    }

    #[test]
    fn the_page_everyone_links_to_outranks_the_one_nobody_does() {
        let scores = site().finish();

        let home = scores["https://example.com/"].authority;
        let privacy = scores["https://example.com/privacy"].authority;
        let about = scores["https://example.com/about"].authority;

        // The claim the ranking depends on: the home page — which is what someone
        // searching for the site's name means — beats a legal page that mentions
        // the name more often.
        assert!(
            home > privacy,
            "home {home} should outrank privacy {privacy}"
        );
        assert!(home > about, "home {home} should outrank about {about}");
    }

    #[test]
    fn scores_stay_inside_their_range() {
        for (_, score) in site().finish() {
            assert!((0.0..=1.0).contains(&score.authority), "{score:?}");
            assert!((0.0..=1.0).contains(&score.host_authority), "{score:?}");
        }
    }

    #[test]
    fn a_page_with_no_inlinks_still_has_a_score_rather_than_none() {
        // Every crawled page must be scored, or a document would silently get a
        // zero through a missing map entry rather than through the graph saying so.
        let scores = site().finish();
        assert_eq!(scores.len(), 7);
        assert!(scores.values().all(|score| score.authority > 0.0));
    }

    #[test]
    fn cross_site_links_count_and_internal_ones_do_not() {
        let mut graph = graph();
        graph.record(
            "https://blog.example/post",
            &links(&["https://example.com/", "https://example.com/docs"]),
        );
        graph.record(
            "https://example.com/",
            &links(&["https://example.com/docs"]),
        );

        let scores = graph.finish();

        // Two links arrive at example.com from elsewhere, so it has host authority.
        assert!(scores["https://example.com/"].host_authority > 0.0);
        // The blog is linked from nowhere, and its own link out is not authority.
        assert_eq!(scores["https://blog.example/post"].host_authority, 0.0);
        // The host the links came *from* gets nothing for pointing at someone else.
        assert!(!graph.host_inlinks().contains_key("blog.example"));
        assert_eq!(graph.host_inlinks().get("example.com"), Some(&2));
    }

    #[test]
    fn a_link_to_an_uncrawled_page_still_gives_its_host_credit() {
        let mut graph = graph();
        graph.record(
            "https://example.com/",
            &links(&["https://not-crawled.example/deep/page"]),
        );

        let scores = graph.finish();

        // `not-crawled.example` is a node with no document behind it, so it has no
        // page score to report, but it is on the record as being linked to.
        assert_eq!(graph.host_inlinks().get("not-crawled.example"), Some(&1));
        assert!(scores.contains_key("https://not-crawled.example/deep/page"));
    }

    #[test]
    fn repeated_links_from_one_page_count_once() {
        let mut graph = graph();
        graph.record(
            "https://example.com/a",
            &links(&[
                "https://other.example/x",
                "https://other.example/x",
                "https://other.example/x",
            ]),
        );
        graph.finish();

        assert_eq!(graph.host_inlinks().get("other.example"), Some(&1));
    }

    #[test]
    fn counts_accumulate_across_runs() {
        let mut previous = BTreeMap::new();
        previous.insert("example.com".to_string(), 5u64);

        let mut graph = LinkGraph::new(GraphLimits::default(), previous);
        graph.record("https://blog.example/a", &links(&["https://example.com/x"]));
        graph.finish();

        // The point of persisting them: authority is a running total over the whole
        // crawl history, not a fact about one drain.
        assert_eq!(graph.host_inlinks().get("example.com"), Some(&6));
    }

    #[test]
    fn the_rank_vector_keeps_its_mass_even_with_dead_ends() {
        // A leaky implementation shrinks every score by however much of the corpus
        // happens to be link-less, which would make the number a fact about the
        // corpus rather than about the page.
        let mut graph = graph();
        graph.record("https://a.example/1", &links(&["https://a.example/2"]));
        graph.record("https://a.example/2", &links(&[]));
        graph.record("https://a.example/3", &links(&[]));
        graph.record("https://a.example/4", &links(&[]));

        let total: f32 = graph.page_rank().iter().sum();
        assert!((total - 1.0).abs() < 1e-3, "rank mass was {total}");
    }

    #[test]
    fn an_empty_graph_scores_nothing_rather_than_dividing_by_zero() {
        let mut graph = graph();
        assert!(graph.finish().is_empty());
        assert_eq!(graph.page_rank(), Vec::<f32>::new());
    }

    #[test]
    fn a_graph_that_hits_its_edge_budget_says_so() {
        let mut graph = LinkGraph::new(
            GraphLimits {
                max_nodes: 100,
                max_edges: 3,
            },
            BTreeMap::new(),
        );
        graph.record(
            "https://example.com/",
            &links(&[
                "https://a.example/x",
                "https://b.example/x",
                "https://c.example/x",
                "https://d.example/x",
            ]),
        );

        // Half an authority signal that claims to be whole is worse than a smaller
        // one that admits it, so the flag has to be set for the caller to log.
        assert!(graph.truncated());
        assert_eq!(graph.shape().1, 3);
    }

    #[test]
    fn the_same_page_recorded_twice_keeps_its_last_view_of_its_links() {
        let mut graph = graph();
        graph.record("https://example.com/", &links(&["https://a.example/1"]));
        graph.record("https://example.com/", &links(&["https://b.example/1"]));

        assert_eq!(graph.shape(), (3, 1));
        graph.finish();
        assert_eq!(graph.host_inlinks().get("a.example"), None);
        assert_eq!(graph.host_inlinks().get("b.example"), Some(&1));
    }

    #[test]
    fn squashing_is_monotonic_and_bounded() {
        assert_eq!(squash(0.0, 4.0), 0.0);
        assert!(squash(1.0, 4.0) < squash(4.0, 4.0));
        assert!(squash(4.0, 4.0) < squash(400.0, 4.0));
        assert!(squash(400.0, 4.0) <= 1.0);
        // A negative input is not a thing, but must not produce a negative score.
        assert_eq!(squash(-5.0, 4.0), 0.0);
        assert_eq!(squash(5.0, 0.0), 0.0);
    }

    #[test]
    fn hosts_are_extracted_from_the_urls_the_crawler_actually_stores() {
        assert_eq!(
            host_of("https://www.amazon.com/dp/B0?x=1#f"),
            Some("www.amazon.com".to_string())
        );
        assert_eq!(
            host_of("http://127.0.0.1:8098/p10.html"),
            Some("127.0.0.1:8098".to_string())
        );
        assert_eq!(host_of("nonsense"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn host_counts_survive_a_save_and_load() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("hosts.json");

        let mut counts = BTreeMap::new();
        counts.insert("example.com".to_string(), 12u64);
        save_host_inlinks(&path, &counts).expect("save");

        assert_eq!(load_host_inlinks(&path), counts);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_missing_or_corrupt_history_is_an_empty_start_not_a_failure() {
        let directory = tempfile::tempdir().expect("tempdir");
        let missing = directory.path().join("nope.json");
        assert!(load_host_inlinks(&missing).is_empty());

        let corrupt = directory.path().join("broken.json");
        std::fs::write(&corrupt, "{ this is not json").expect("write");
        assert!(load_host_inlinks(&corrupt).is_empty());
    }
}
