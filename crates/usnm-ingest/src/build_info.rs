//! What built an index version (#161): the code, the search engine, the
//! feature versions and the index templates, so versions can be compared
//! later without working out from merge times what each one had.
//!
//! A release records it in two places: the version's manifest
//! (`{version}/manifest.json` `build`, with the templates in full) and its
//! `index_runs` item (`build`, with the templates' checksums only, so the
//! item stays small). It describes what this run built: the templates it
//! applied are those of the indexes it wrote. A delta's base, and the main
//! indexes an overlay-only release keeps, were built by earlier versions:
//! follow `previous_version` to their records.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::source::hex;

/// The commit the image was built from (`ARG USNM_GIT_SHA` in
/// `Dockerfile.ingest`, set by CI). Absent outside CI builds.
const COMMIT_ENV: &str = "USNM_GIT_SHA";

/// What a run builds: which indexes it writes, on which engine.
#[derive(Debug, Clone, Default)]
pub struct Built {
    pub full: bool,
    /// It writes a main index (`pages-…`): every release but an overlay-only one.
    pub main_index: bool,
    /// It writes a Japanese index (`pages-ja-…`).
    pub ja_index: bool,
    /// The engine, from the sink (`IndexSink::engine`).
    pub engine: Option<String>,
    /// The writer's tuning for this run (#172): its main index template is
    /// the tuned one, and the record says so.
    pub writer: crate::sink::WriterTuning,
    /// The version has American Stories' text (`--american-stories`): the
    /// main index this run writes has it, and so do the ones it keeps.
    pub american_stories: bool,
    /// The release writes our Japanese OCR's Latin text for the pages LoC
    /// ships without text (`--ja-latin`, #203).
    pub ja_latin: bool,
    /// The decade layout (05 §5.5.5): of the main index this run writes,
    /// and of the version's main indexes, which all have the `decade` field
    /// or none does.
    pub decades: crate::sink::Decades,
    /// The text fields (05 §5.5.7): of the main index this run writes, and
    /// of the version's main indexes, which all have the same ones.
    pub text_layout: usnm_core::text_layout::TextLayout,
    /// The analyzer the run's indexes fold words with (#168): the latest for
    /// a full base, the published version's for a delta. The record's
    /// `common_grams` and `ja_fold` name it.
    pub analyzer: usnm_core::text::Analyzer,
}

/// The index templates the run applies, by name.
fn templates(b: &Built) -> Vec<(&'static str, String)> {
    let mut t = Vec::new();
    if b.main_index {
        // As the writer applies it: the run's tuning in place of the
        // template's heap and commit timeout, its decade layout and its
        // text fields.
        let layout = crate::sink::MainLayout {
            decades: b.decades,
            text: b.text_layout,
        };
        let pages = crate::sink::main_template(&b.writer, layout)
            .unwrap_or_else(|_| crate::sink::INDEX_TEMPLATE.to_owned());
        t.push(("pages", pages));
    }
    if b.ja_index {
        t.push(("pages-ja", crate::ocr_ja::JA_TEMPLATE.to_owned()));
    }
    t
}

/// The full record, for the manifest: templates in full.
pub fn record(b: &Built) -> Value {
    let mut v = summary(b);
    for (name, yaml) in templates(b) {
        v["templates"][name]["yaml"] = Value::from(yaml);
    }
    v
}

/// The record without the templates' text, for the `index_runs` item.
pub fn summary(b: &Built) -> Value {
    let mut templates_v = json!({});
    for (name, yaml) in templates(b) {
        templates_v[name] = json!({ "sha256": hex(&Sha256::digest(yaml.as_bytes())) });
    }
    // What the writer was given (#172). The queue is the node's; the heap
    // and commit timeout are only the main index's (the Japanese template
    // keeps its own), so an overlay-only run doesn't record them.
    let mut writer = json!({ "queue": b.writer.queue });
    if b.main_index {
        writer["heap"] = Value::from(b.writer.heap.clone());
        writer["commit_timeout_secs"] = Value::from(b.writer.commit_timeout_secs);
    }
    let mut features = json!({
        "common_grams": usnm_core::common_grams::version(b.analyzer),
        "ja_fold": usnm_core::ja::fold_version(b.analyzer),
    });
    // Only for a version with it, as in its `current.json`.
    if b.american_stories {
        features["american_stories"] = json!(usnm_core::american_stories::VERSION);
    }
    if b.ja_latin {
        features["ja_latin"] = json!(crate::ocr_ja::LATIN_VERSION);
    }
    // Likewise, as its `decades`.
    if b.decades.on() {
        features["decades"] = json!(usnm_core::decade::VERSION);
    }
    // Likewise, as its `text_layout`.
    if b.text_layout != usnm_core::text_layout::TextLayout::Separate {
        features["text_layout"] = json!(b.text_layout.version());
    }
    json!({
        "commit": std::env::var(COMMIT_ENV).ok().filter(|s| !s.is_empty()),
        "ingest": env!("CARGO_PKG_VERSION"),
        "engine": b.engine,
        "full": b.full,
        "features": features,
        "templates": templates_v,
        "writer": writer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built(main_index: bool, ja_index: bool) -> Built {
        Built {
            full: true,
            main_index,
            ja_index,
            engine: Some("Quickwit 0.9.1".into()),
            writer: crate::sink::WriterTuning::default(),
            american_stories: false,
            ja_latin: false,
            decades: crate::sink::Decades::Off,
            text_layout: usnm_core::text_layout::TextLayout::Separate,
            analyzer: usnm_core::text::Analyzer::LATEST,
        }
    }

    #[test]
    fn records_one_text_field_and_its_template() {
        let off = record(&built(true, false));
        assert!(off["features"].get("text_layout").is_none(), "{off}");
        let mut b = built(true, false);
        b.text_layout = usnm_core::text_layout::TextLayout::Single;
        let v = record(&b);
        assert_eq!(v["features"]["text_layout"], 2);
        let yaml = v["templates"]["pages"]["yaml"].as_str().unwrap();
        assert!(yaml.contains("name: text_all,") && !yaml.contains("name: text_cg,"));
    }

    #[test]
    fn records_the_analyzer_versions_the_run_built_with() {
        let mut b = built(true, true);
        b.analyzer = usnm_core::text::Analyzer::V1;
        let v = summary(&b);
        assert_eq!(v["features"]["common_grams"], 1);
        assert_eq!(v["features"]["ja_fold"], 1);
    }

    #[test]
    fn records_the_latin_text_only_for_a_version_with_it() {
        let v = summary(&built(true, false));
        assert!(v["features"].get("ja_latin").is_none(), "{v}");
        let mut b = built(true, false);
        b.ja_latin = true;
        assert_eq!(
            summary(&b)["features"]["ja_latin"],
            crate::ocr_ja::LATIN_VERSION
        );
    }

    #[test]
    fn records_american_stories_only_for_a_version_with_it() {
        let v = summary(&built(true, false));
        assert!(v["features"].get("american_stories").is_none(), "{v}");
        let mut b = built(true, false);
        b.american_stories = true;
        let v = record(&b);
        assert_eq!(
            v["features"]["american_stories"],
            usnm_core::american_stories::VERSION
        );
        assert_eq!(
            v["features"]["common_grams"],
            usnm_core::common_grams::VERSION
        );
    }

    #[test]
    fn records_the_decade_layout_and_its_template() {
        let off = record(&built(true, false));
        assert!(off["features"].get("decades").is_none(), "{off}");
        let mut b = built(true, false);
        b.decades = crate::sink::Decades::Partitioned;
        let v = record(&b);
        assert_eq!(v["features"]["decades"], usnm_core::decade::VERSION);
        let yaml = v["templates"]["pages"]["yaml"].as_str().unwrap();
        assert!(yaml.contains("partition_key: decade"), "{yaml}");
        b.decades = crate::sink::Decades::Tagged;
        let yaml = record(&b)["templates"]["pages"]["yaml"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(yaml.contains("tag_fields: [decade]") && !yaml.contains("partition_key"));
    }

    #[test]
    fn records_the_tuned_template_and_the_tuning() {
        let mut b = built(true, false);
        b.writer = crate::sink::WriterTuning {
            heap: "6GiB".into(),
            commit_timeout_secs: 600,
            queue: "4GiB".into(),
        };
        let v = record(&b);
        let yaml = v["templates"]["pages"]["yaml"].as_str().unwrap();
        assert!(yaml.contains("heap_size: 6GiB") && yaml.contains("commit_timeout_secs: 600"));
        assert_ne!(
            v["templates"]["pages"]["sha256"],
            record(&built(true, false))["templates"]["pages"]["sha256"]
        );
        assert_eq!(v["writer"]["heap"], "6GiB");
        assert_eq!(v["writer"]["commit_timeout_secs"], 600);
    }

    #[test]
    fn an_overlay_only_run_records_only_the_queue() {
        let mut b = built(false, true);
        b.writer = crate::sink::WriterTuning {
            heap: "6GiB".into(),
            commit_timeout_secs: 600,
            queue: "4GiB".into(),
        };
        let v = summary(&b);
        assert_eq!(v["writer"], json!({ "queue": "4GiB" }));
    }

    #[test]
    fn records_features_and_the_templates_applied() {
        let v = record(&built(true, true));
        assert_eq!(
            (v["full"].clone(), v["engine"].clone()),
            (json!(true), json!("Quickwit 0.9.1"))
        );
        assert_eq!(
            v["features"]["common_grams"],
            usnm_core::common_grams::VERSION
        );
        assert_eq!(v["features"]["ja_fold"], usnm_core::ja::FOLD_VERSION);
        let pages = &v["templates"]["pages"];
        assert_eq!(pages["yaml"], crate::sink::INDEX_TEMPLATE);
        assert_eq!(pages["sha256"].as_str().unwrap().len(), 64);
        assert!(v["templates"]["pages-ja"]["yaml"].is_string());
        // The run item's copy has the checksums only.
        let s = summary(&built(true, false));
        assert_eq!(s["templates"]["pages"]["sha256"], pages["sha256"]);
        assert!(s["templates"]["pages"].get("yaml").is_none());
        assert!(s["templates"].get("pages-ja").is_none());
    }

    #[test]
    fn an_overlay_only_release_applies_only_the_japanese_template() {
        let v = record(&built(false, true));
        assert!(v["templates"].get("pages").is_none());
        assert!(v["templates"]["pages-ja"]["sha256"].is_string());
    }
}
