//! Index schema for the Tantivy backing store.
//!
//! ## Deviations from the field list in the project brief
//!
//! Two fields carry more options than the brief specifies. Both are deliberate,
//! and both are being decided now rather than in Phase 3 because changing a
//! schema means reindexing everything:
//!
//! * `url` is indexed with the `raw` tokenizer (`STRING`), not merely stored.
//!   Phase 3 requires add-or-update by URL, and an upsert needs an indexed
//!   unique key to match on. Without it there is no way to replace a document.
//! * `body` is stored, not only indexed. Phase 3 requires snippets, and
//!   `SnippetGenerator::snippet_from_doc` reads the matched text back out of the
//!   stored document. Storing the body is the single largest contributor to
//!   index size, so this is the first thing to revisit if the footprint budget
//!   gets tight -- the alternative is a separate stored excerpt field.
//!
//! A third, smaller deviation: `fetched_at` is stored as well as indexed fast.
//! The brief asked for `FAST` for sorting, and it still is one. But a fast field
//! cannot be read back per document through the normal document API, and results
//! have to return `fetched_at` for the freshness ranking to work. The alternative
//! is a per-segment fast-field reader on the query hot path; eight stored bytes
//! per document is a cheaper way to buy the same thing without the fragility.

use tantivy::schema::{FAST, Field, STORED, STRING, Schema, TEXT};

/// Field name for the page URL.
pub const FIELD_URL: &str = "url";
/// Field name for the page title.
pub const FIELD_TITLE: &str = "title";
/// Field name for the extracted page text.
pub const FIELD_BODY: &str = "body";
/// Field name for the fetch timestamp.
pub const FIELD_FETCHED_AT: &str = "fetched_at";
/// Field name for how linked-to a page is within the crawl's link graph.
pub const FIELD_AUTHORITY: &str = "authority";
/// Field name for how many *other sites* link to this page's host.
pub const FIELD_HOST_AUTHORITY: &str = "host_authority";

/// The index schema together with the resolved [`Field`] handles.
///
/// Resolving the handles once at construction avoids a name lookup per document
/// on the indexing hot path.
#[derive(Debug, Clone)]
pub struct IndexSchema {
    schema: Schema,
    url: Field,
    title: Field,
    body: Field,
    fetched_at: Field,
    authority: Field,
    host_authority: Field,
}

impl IndexSchema {
    /// Builds the schema. This must stay in step with what is already on disk:
    /// see [`IndexSchema::is_compatible_with`].
    pub fn build() -> Self {
        let mut builder = Schema::builder();
        let url = builder.add_text_field(FIELD_URL, STRING | STORED);
        let title = builder.add_text_field(FIELD_TITLE, TEXT | STORED);
        let body = builder.add_text_field(FIELD_BODY, TEXT | STORED);
        let fetched_at = builder.add_i64_field(FIELD_FETCHED_AT, FAST | STORED);
        // Both are computed by the indexer rather than the crawler: authority is a
        // property of the whole link graph, and the crawler sees one page at a
        // time. They are stored as well as fast for the same reason `fetched_at`
        // is -- results have to return them for the ranking pass to use them.
        // f64 rather than f32: Tantivy 0.26 offers no f32 field, and the values are
        // in [0, 1] anyway, so the wider type costs nothing that matters.
        let authority = builder.add_f64_field(FIELD_AUTHORITY, FAST | STORED);
        let host_authority = builder.add_f64_field(FIELD_HOST_AUTHORITY, FAST | STORED);
        Self {
            schema: builder.build(),
            url,
            title,
            body,
            fetched_at,
            authority,
            host_authority,
        }
    }

    /// The underlying Tantivy schema, for handing to `Index::create`.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Field handle for the page URL.
    pub fn url_field(&self) -> Field {
        self.url
    }

    /// Field handle for the page title.
    pub fn title_field(&self) -> Field {
        self.title
    }

    /// Field handle for the page body.
    pub fn body_field(&self) -> Field {
        self.body
    }

    /// Field handle for the fetch timestamp.
    pub fn fetched_at_field(&self) -> Field {
        self.fetched_at
    }

    /// Field handle for the page's link-graph authority.
    pub fn authority_field(&self) -> Field {
        self.authority
    }

    /// Field handle for the host's cross-site authority.
    pub fn host_authority_field(&self) -> Field {
        self.host_authority
    }

    /// Whether an already-open index was built with a schema this code can read.
    ///
    /// Called on open by both the writer and the query engine, which refuse to
    /// start on a mismatch. A silently incompatible schema produces wrong results
    /// rather than errors, and that is far more expensive to diagnose than a
    /// refusal to start.
    pub fn is_compatible_with(&self, other: &Schema) -> bool {
        for name in [
            FIELD_URL,
            FIELD_TITLE,
            FIELD_BODY,
            FIELD_FETCHED_AT,
            FIELD_AUTHORITY,
            FIELD_HOST_AUTHORITY,
        ] {
            let (Ok(mine), Ok(theirs)) = (self.schema.get_field(name), other.get_field(name))
            else {
                return false;
            };
            if self.schema.get_field_entry(mine).field_type()
                != other.get_field_entry(theirs).field_type()
            {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::{FieldType, TextOptions};

    fn text_options(schema: &Schema, field: Field) -> TextOptions {
        match schema.get_field_entry(field).field_type() {
            FieldType::Str(options) => options.clone(),
            other => panic!("expected a text field, got {other:?}"),
        }
    }

    fn assert_default_analyzer(schema: &Schema, field: Field) {
        let options = text_options(schema, field);
        let indexing = options
            .get_indexing_options()
            .expect("field must be indexed to be searchable");
        assert_eq!(indexing.tokenizer(), "default");
    }

    #[test]
    fn schema_contains_exactly_the_expected_fields() {
        let schema = IndexSchema::build();
        assert_eq!(schema.schema().num_fields(), 6);

        // Resolved handles must agree with lookup by name, or every write and
        // read would be against a different field than intended.
        assert_eq!(
            schema.schema().get_field(FIELD_URL).unwrap(),
            schema.url_field()
        );
        assert_eq!(
            schema.schema().get_field(FIELD_TITLE).unwrap(),
            schema.title_field()
        );
        assert_eq!(
            schema.schema().get_field(FIELD_BODY).unwrap(),
            schema.body_field()
        );
        assert_eq!(
            schema.schema().get_field(FIELD_FETCHED_AT).unwrap(),
            schema.fetched_at_field()
        );
        assert_eq!(
            schema.schema().get_field(FIELD_AUTHORITY).unwrap(),
            schema.authority_field()
        );
        assert_eq!(
            schema.schema().get_field(FIELD_HOST_AUTHORITY).unwrap(),
            schema.host_authority_field()
        );
    }

    #[test]
    fn authority_fields_are_readable_and_sortable() {
        let schema = IndexSchema::build();
        for field in [schema.authority_field(), schema.host_authority_field()] {
            match schema.schema().get_field_entry(field).field_type() {
                FieldType::F64(options) => {
                    assert!(options.is_fast(), "ranking sorts on this");
                    assert!(
                        options.is_stored(),
                        "results return it, and a fast field alone cannot be read back per document"
                    );
                }
                other => panic!("expected an f64 field, got {other:?}"),
            }
        }
    }

    #[test]
    fn url_is_raw_indexed_and_stored_so_upserts_can_match_on_it() {
        let schema = IndexSchema::build();
        let options = text_options(schema.schema(), schema.url_field());

        assert!(options.is_stored(), "results must return the url");
        let indexing = options
            .get_indexing_options()
            .expect("url must be indexed or upsert-by-url is impossible");
        // The raw tokenizer keeps the URL as a single token, which is what makes
        // an exact term match possible. With the default analyzer a URL would be
        // split on punctuation and could never be matched exactly.
        assert_eq!(indexing.tokenizer(), "raw");
    }

    #[test]
    fn title_and_body_are_full_text_searchable() {
        let schema = IndexSchema::build();
        assert_default_analyzer(schema.schema(), schema.title_field());
        assert_default_analyzer(schema.schema(), schema.body_field());
    }

    #[test]
    fn body_is_stored_because_snippet_generation_reads_it_back() {
        let schema = IndexSchema::build();
        assert!(text_options(schema.schema(), schema.body_field()).is_stored());
    }
    #[test]
    fn fetched_at_is_both_sortable_and_returnable() {
        let schema = IndexSchema::build();

        match schema
            .schema()
            .get_field_entry(schema.fetched_at_field())
            .field_type()
        {
            FieldType::I64(options) => {
                assert!(options.is_fast(), "freshness sorting needs a fast field");
                assert!(
                    options.is_stored(),
                    "results return fetched_at, and a fast field alone cannot be read back per document"
                );
            }
            other => panic!("expected an i64 field, got {other:?}"),
        }
    }

    #[test]
    fn an_index_missing_a_field_is_rejected() {
        // Forgetting a field would otherwise mean queries against it silently
        // match nothing.
        let mut builder = Schema::builder();
        builder.add_text_field(FIELD_URL, STRING | STORED);
        builder.add_text_field(FIELD_TITLE, TEXT | STORED);
        let incomplete = builder.build();

        assert!(!IndexSchema::build().is_compatible_with(&incomplete));
    }

    #[test]
    fn an_index_with_a_differently_configured_field_is_rejected() {
        // The subtle case: same field names, different options. `body` without
        // STORED could never produce a snippet, and `url` without the raw
        // tokenizer could never match an upsert.
        let mut builder = Schema::builder();
        builder.add_text_field(FIELD_URL, TEXT | STORED);
        builder.add_text_field(FIELD_TITLE, TEXT | STORED);
        builder.add_text_field(FIELD_BODY, TEXT | STORED);
        builder.add_i64_field(FIELD_FETCHED_AT, FAST | STORED);
        let mismatched = builder.build();

        assert!(!IndexSchema::build().is_compatible_with(&mismatched));
    }

    #[test]
    fn rebuilding_the_schema_is_deterministic() {
        // Two processes (the indexer and the API) build this independently. If
        // the field order or ids could differ between them, the API would read
        // the wrong columns.
        let a = IndexSchema::build();
        let b = IndexSchema::build();
        assert_eq!(a.url_field(), b.url_field());
        assert_eq!(a.title_field(), b.title_field());
        assert_eq!(a.body_field(), b.body_field());
        assert_eq!(a.fetched_at_field(), b.fetched_at_field());
        assert_eq!(a.authority_field(), b.authority_field());
        assert_eq!(a.host_authority_field(), b.host_authority_field());
        assert!(a.is_compatible_with(b.schema()));
    }

    #[test]
    fn an_index_built_before_authority_existed_is_rejected() {
        // The guard that makes adding a field safe: an old index has to be
        // reindexed rather than quietly answered from without the new column.
        let mut builder = Schema::builder();
        builder.add_text_field(FIELD_URL, STRING | STORED);
        builder.add_text_field(FIELD_TITLE, TEXT | STORED);
        builder.add_text_field(FIELD_BODY, TEXT | STORED);
        builder.add_i64_field(FIELD_FETCHED_AT, FAST | STORED);
        let older = builder.build();

        assert!(!IndexSchema::build().is_compatible_with(&older));
    }
}
