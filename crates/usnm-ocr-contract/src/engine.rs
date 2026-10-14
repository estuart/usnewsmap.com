//! The engine contract, `usnm-ocr-engine/1` (ADR-0016, 13 §13.4): JSON
//! lines between the worker and an engine adapter on the adapter's stdin
//! and stdout. These types are the source of truth; `ocr/contract/v1/`
//! holds the JSON Schemas generated from them for adapters in other
//! languages (`usnm-ocr-contract-schema`, checked in CI).
//!
//! - The adapter sends one [`Hello`] when it is ready.
//! - The worker sends one [`PageRequest`] at a time, and the adapter
//!   answers each with one [`PageResult`].
//! - Images and outputs go through files: the image in the read-only
//!   `/in`, the adapter's ALTO or `boxes.json` (`usnm_alto::boxes`) and
//!   text in `/out/<run_id>/`.
//!
//! Every message is one JSON object on one line, at most [`MAX_LINE`]
//! bytes. Within a contract major version a reader ignores fields it
//! doesn't know, so no type here denies unknown fields. Fields are
//! declared in alphabetical order, so the lines' keys come out sorted.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
pub use usnm_alto::{Granularity, Markup};

/// The contract's major version, sent as `--contract 1` and in `hello`.
pub const CONTRACT: u32 = 1;
/// The contract versions this code speaks: N and, during a move to a new
/// major version, N-1 (13 §13.4.8).
pub const SUPPORTED: &[u32] = &[CONTRACT];
/// The longest line either side may send, newline included.
pub const MAX_LINE: usize = 1 << 20;
/// A failure's message (13 §13.4.4).
pub const MAX_ERROR_MESSAGE: usize = 2 << 10;
/// `engine_stats`: keys and key length.
pub const MAX_STATS: usize = 64;
pub const MAX_STAT_KEY: usize = 64;

/// Size caps on the engine's files (13 §13.4.7 step 1). Engine ALTO is
/// more verbose than the stored ALTO, so these exceed [`limits`].
pub mod engine_files {
    pub const LAYOUT: u64 = 64 << 20;
    pub const TEXT: u64 = 8 << 20;
    pub const JOB_DIR: u64 = 256 << 20;
}

/// What a node may upload for one page (13 §13.4.7 step 7; `ocr-sync`'s
/// checks 1 and 4 of 12 Step 8). The worker and the pickup share them, so a
/// page the worker uploads can't be quarantined for its size.
pub mod limits {
    pub const STORED_ALTO: usize = 32 << 20;
    pub const PART: usize = 1 << 20;
    pub const RUNS: usize = 1 << 20;
    pub const TEXT: usize = 512 << 10;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Adapter {
    /// `rust` or `python`.
    pub lang: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct Component {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EngineInfo {
    /// Libraries such as Leptonica, with their versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<Component>>,
    /// `usnm-<family>` is the rows' `ocr_source`.
    pub family: String,
    /// The engine's id in `engines.toml`.
    pub id: String,
    pub version: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct Model {
    /// The file's path inside the image, e.g. `tessdata/deu.traineddata`.
    pub name: String,
    /// Hashed from disk when the adapter starts.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Profile {
    pub id: String,
    /// ISO 639-3.
    pub languages: Vec<String>,
    /// Names from [`Hello::models`].
    pub models: Vec<String>,
    /// The options a route may set.
    pub tunable: Vec<String>,
}

/// How a page that runs past its deadline is stopped (13 §13.6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cancel {
    /// The adapter stops at `deadline_ms`, kills what it started and
    /// answers `timeout`.
    Adapter,
    /// The worker kills the instance.
    Kill,
}

/// The engine's layout output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// ALTO, any version, any `MeasurementUnit`.
    Alto,
    /// `boxes.json` (`usnm-boxes/1`).
    Boxes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelsLoaded {
    /// Kept in memory between pages.
    AtStart,
    /// Loaded for each page (CLI and script engines): the load is part of
    /// the page's time.
    PerPage,
}

/// The adapter's first line (13 §13.4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Hello {
    pub adapter: Adapter,
    pub cancel: Cancel,
    pub contract: u32,
    pub deterministic: bool,
    pub engine: EngineInfo,
    /// The finest level with real geometry.
    pub granularity: Granularity,
    pub layout: Layout,
    /// Every model file any profile can load.
    pub models: Vec<Model>,
    pub models_loaded: ModelsLoaded,
    pub profiles: Vec<Profile>,
}

impl Hello {
    /// Whether `self` is the same `hello` as `pinned` in everything the
    /// lock pins (13 §13.4.2): every field, models as a set, and profiles
    /// by id with their languages, models and tunable options as sets.
    pub fn matches(&self, pinned: &Hello) -> bool {
        let comps = |h: &Hello| {
            h.engine
                .components
                .as_ref()
                .map(|c| c.iter().cloned().collect::<BTreeSet<_>>())
        };
        let models = |h: &Hello| h.models.iter().cloned().collect::<BTreeSet<_>>();
        type ProfileSets = (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>);
        let profiles = |h: &Hello| -> Option<BTreeMap<String, ProfileSets>> {
            let mut m = BTreeMap::new();
            for p in &h.profiles {
                let set = |v: &Vec<String>| v.iter().cloned().collect::<BTreeSet<_>>();
                if m.insert(
                    p.id.clone(),
                    (set(&p.languages), set(&p.models), set(&p.tunable)),
                )
                .is_some()
                {
                    return None;
                }
            }
            Some(m)
        };
        self.adapter == pinned.adapter
            && self.contract == pinned.contract
            && self.engine.family == pinned.engine.family
            && self.engine.id == pinned.engine.id
            && self.engine.version == pinned.engine.version
            && comps(self) == comps(pinned)
            && self.cancel == pinned.cancel
            && self.deterministic == pinned.deterministic
            && self.granularity == pinned.granularity
            && self.layout == pinned.layout
            && self.models_loaded == pinned.models_loaded
            && models(self) == models(pinned)
            && profiles(self).is_some()
            && profiles(self) == profiles(pinned)
    }

    /// Checks what the worker refuses an instance for before the lock
    /// comparison: a contract version it doesn't speak, an empty engine
    /// family, id or version, or two profiles with one id.
    pub fn check(&self) -> Result<(), ProtocolError> {
        if !SUPPORTED.contains(&self.contract) {
            return Err(ProtocolError::Contract(self.contract));
        }
        let e = &self.engine;
        if e.family.is_empty() || e.id.is_empty() || e.version.is_empty() {
            return Err(ProtocolError::Hello(
                "an empty engine family, id or version",
            ));
        }
        let ids: BTreeSet<&str> = self.profiles.iter().map(|p| p.id.as_str()).collect();
        if ids.len() != self.profiles.len() {
            return Err(ProtocolError::Hello("two profiles with one id"));
        }
        Ok(())
    }

    /// The profile `id`, if this image has it.
    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DpiSource {
    /// IIIF JPEGs carry no resolution: 300 for a full scan.
    Assumed,
    /// Read from LoC's ALTO.
    LocAlto,
}

/// The file the adapter reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Image {
    /// The scan's dpi times the worker's scale.
    pub dpi: u32,
    pub dpi_source: DpiSource,
    pub height: u32,
    /// `/in/<run_id>.jpg`, or `.png` when the worker scaled the scan or
    /// cut a tile.
    pub path: String,
    pub sha256: String,
    pub width: u32,
}

/// A page for the adapter (13 §13.4.3). It carries no page id, URL, batch
/// or credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PageRequest {
    pub deadline_ms: u64,
    pub image: Image,
    /// ISO 639-3.
    pub language: String,
    /// The profile's defaults overlaid by the route's options.
    pub options: BTreeMap<String, serde_json::Value>,
    /// `/out/<run_id>/`.
    pub out: String,
    pub profile: String,
    pub run_id: String,
    /// ISO 15924.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// Profile, language or option not in this image.
    Unsupported,
    /// The engine can't read an image the worker decoded.
    BadInput,
    Failed,
    /// The adapter stopped at `deadline_ms` (`cancel = adapter`).
    Timeout,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Unsupported => "unsupported",
            Status::BadInput => "bad_input",
            Status::Failed => "failed",
            Status::Timeout => "timeout",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LayoutFile {
    /// For ALTO the engine converted from pixels at an assumed dpi (NDNP
    /// Open OCR at 300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alto_dpi: Option<f64>,
    pub kind: Layout,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TextFile {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResultError {
    pub kind: String,
    /// At most 2 KiB, never page text.
    pub message: String,
}

/// The adapter's answer to one page (13 §13.4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PageResult {
    /// Free-form numbers the worker keeps and never interprets.
    #[serde(default)]
    pub engine_stats: BTreeMap<String, serde_json::Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResultError>,
    /// Present when `status` is `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutFile>,
    #[serde(default)]
    pub markup: Markup,
    /// Text read before a deadline; only the survey reads it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// With `failed`: the adapter knows its engine is in a bad state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restart: bool,
    pub run_id: String,
    pub status: Status,
    /// The engine's own text, which the survey compares. The stored text
    /// is always derived from the layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextFile>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A line from the adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromAdapter {
    Hello(Hello),
    Result(PageResult),
}

/// A line from the worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToAdapter {
    Page(PageRequest),
}

/// A broken protocol: the worker's `engine_protocol` (13 §13.4.6).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("a line over {MAX_LINE} bytes")]
    TooLong,
    #[error("contract version {0}, not one of {SUPPORTED:?}")]
    Contract(u32),
    #[error("a bad hello: {0}")]
    Hello(&'static str),
    #[error("a bad page request: {0}")]
    Page(&'static str),
    #[error("a line that isn't contract JSON: {0}")]
    NotContract(String),
    #[error("a result for run {got}, not {want}")]
    WrongRun { got: String, want: String },
    #[error("layout {got:?}, but hello declared {want:?}")]
    WrongLayout { got: Layout, want: Layout },
    #[error("status ok with no layout file")]
    NoLayout,
    #[error("a failure with no error")]
    NoError,
    #[error("restart on a status other than failed")]
    Restart,
    #[error("an error message over {MAX_ERROR_MESSAGE} bytes")]
    ErrorTooLong,
    #[error("engine_stats over {MAX_STATS} keys or a key over {MAX_STAT_KEY} characters")]
    Stats,
    #[error("partial output from production")]
    Partial,
    #[error("a file path outside {0}")]
    Path(String),
}

/// One message as a line: compact JSON and a newline.
/// Keys come out sorted, as in 13's examples.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let value = serde_json::to_value(msg).map(sort_keys).unwrap_or_default();
    let mut v = serde_json::to_vec(&value).unwrap_or_default();
    v.push(b'\n');
    v
}

/// One line from an adapter, within [`MAX_LINE`].
pub fn decode(line: &[u8]) -> Result<FromAdapter, ProtocolError> {
    if line.len() > MAX_LINE {
        return Err(ProtocolError::TooLong);
    }
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    serde_json::from_slice(line).map_err(|e| ProtocolError::NotContract(e.to_string()))
}

/// One line from the worker (the adapter's side).
pub fn decode_request(line: &[u8]) -> Result<ToAdapter, ProtocolError> {
    if line.len() > MAX_LINE {
        return Err(ProtocolError::TooLong);
    }
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    serde_json::from_slice(line).map_err(|e| ProtocolError::NotContract(e.to_string()))
}

impl PageRequest {
    /// Checks a request before it is sent: a deadline, an image with a
    /// size and dpi, and `out` the directory `<out_root>/<run_id>/`
    /// (`/out` in a container).
    pub fn check(&self, out_root: &str) -> Result<(), ProtocolError> {
        if self.deadline_ms == 0 {
            return Err(ProtocolError::Page("no time left before the deadline"));
        }
        let i = &self.image;
        if i.width == 0 || i.height == 0 || i.dpi == 0 {
            return Err(ProtocolError::Page("an image with no size or dpi"));
        }
        if self.run_id.is_empty()
            || self.run_id.contains('/')
            || self.out.trim_end_matches('/')
                != format!("{}/{}", out_root.trim_end_matches('/'), self.run_id)
        {
            return Err(ProtocolError::Page("out isn't <out root>/<run_id>/"));
        }
        Ok(())
    }
}

impl PageResult {
    /// Checks a result against its request and the instance's `hello`
    /// (13 §13.4.4): the run, the layout kind, the limits, and that every
    /// file it names is directly in the request's `out` directory. Partial
    /// output is refused unless `survey`.
    pub fn check(
        &self,
        req: &PageRequest,
        hello: &Hello,
        survey: bool,
    ) -> Result<(), ProtocolError> {
        if self.run_id != req.run_id {
            return Err(ProtocolError::WrongRun {
                got: self.run_id.clone(),
                want: req.run_id.clone(),
            });
        }
        if self
            .error
            .as_ref()
            .is_some_and(|e| e.message.len() > MAX_ERROR_MESSAGE)
        {
            return Err(ProtocolError::ErrorTooLong);
        }
        if self.engine_stats.len() > MAX_STATS
            || self
                .engine_stats
                .keys()
                .any(|k| k.chars().count() > MAX_STAT_KEY)
        {
            return Err(ProtocolError::Stats);
        }
        if self.partial && !survey {
            return Err(ProtocolError::Partial);
        }
        if let Some(l) = &self.layout {
            if l.kind != hello.layout {
                return Err(ProtocolError::WrongLayout {
                    got: l.kind,
                    want: hello.layout,
                });
            }
        }
        if self.status == Status::Ok && self.layout.is_none() {
            return Err(ProtocolError::NoLayout);
        }
        // Every failure says why; only `failed` may ask for a restart.
        if self.status != Status::Ok && self.error.is_none() {
            return Err(ProtocolError::NoError);
        }
        if self.restart && self.status != Status::Failed {
            return Err(ProtocolError::Restart);
        }
        let paths = self
            .layout
            .iter()
            .map(|l| l.path.as_str())
            .chain(self.text.iter().map(|t| t.path.as_str()));
        for p in paths {
            if file_name_in(&req.out, p).is_none() {
                return Err(ProtocolError::Path(req.out.clone()));
            }
        }
        Ok(())
    }
}

/// The file name of `path` when it names a file directly inside the
/// directory `out` (`/out/<run_id>/`): no `..`, `.`, empty or nested part.
/// The worker still opens it without following symlinks.
pub fn file_name_in<'a>(out: &str, path: &'a str) -> Option<&'a str> {
    let dir = out.trim_end_matches('/');
    let name = path.strip_prefix(dir)?.strip_prefix('/')?;
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0');
    ok.then_some(name)
}

/// The JSON Schemas of the contract's messages, by file name
/// (`ocr/contract/v1/<name>.schema.json`), each pretty-printed with its
/// keys sorted and a final newline.
pub fn schemas() -> Vec<(&'static str, String)> {
    let gen = |s: schemars::Schema| {
        let v = sort_keys(s.to_value());
        let mut out = serde_json::to_string_pretty(&v).unwrap_or_default();
        out.push('\n');
        out
    };
    vec![
        ("boxes", gen(schemars::schema_for!(usnm_alto::boxes::Boxes))),
        ("hello", gen(tagged::<Hello>("hello"))),
        ("page", gen(tagged::<PageRequest>("page"))),
        ("result", gen(tagged::<PageResult>("result"))),
    ]
}

/// `T`'s schema with the `type` tag the line carries.
fn tagged<T: JsonSchema>(tag: &str) -> schemars::Schema {
    let mut s = schemars::schema_for!(T);
    if let Some(obj) = s.as_object_mut() {
        if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
            props.insert(
                "type".into(),
                serde_json::json!({ "const": tag, "type": "string" }),
            );
        }
        if let Some(req) = obj.get_mut("required").and_then(|r| r.as_array_mut()) {
            req.push("type".into());
            req.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
    }
    s
}

fn sort_keys(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let sorted: BTreeMap<String, serde_json::Value> =
                m.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(sort_keys).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hello of 13 §13.4.2.
    pub(crate) const HELLO: &str = r#"{"adapter":{"lang":"rust","name":"usnm-ocr-adapter-tesseract","version":"0.1.0"},"cancel":"adapter","contract":1,"deterministic":true,"engine":{"family":"tesseract","id":"tesseract-5.5","version":"5.5.3"},"granularity":"word","layout":"alto","models":[{"name":"tessdata/deu.traineddata","sha256":"8407331d6aa0229dc927685c01a7938fc5a641d1a9524f74838cdac599f0d06e"},{"name":"tessdata/deu_latf.traineddata","sha256":"429b8c33d37e1a4823a2bb082a644ff13e252f735b7945e08a518d22ddcc459e"}],"models_loaded":"per_page","profiles":[{"id":"deu","languages":["deu"],"models":["tessdata/deu.traineddata"],"tunable":["psm"]},{"id":"deu_latf+deu","languages":["deu"],"models":["tessdata/deu_latf.traineddata","tessdata/deu.traineddata"],"tunable":["psm"]}],"type":"hello"}"#;

    const PAGE: &str = r#"{"deadline_ms":840000,"image":{"dpi":300,"dpi_source":"assumed","height":7208,"path":"/in/nuc1.20261020T140312123456Z.jpg","sha256":"ab","width":5412},"language":"deu","options":{"psm":3},"out":"/out/nuc1.20261020T140312123456Z/","profile":"deu_latf+deu","run_id":"nuc1.20261020T140312123456Z","script":"Latf","type":"page"}"#;

    const RESULT: &str = r#"{"engine_stats":{"mean_conf":0.71,"words":6034},"layout":{"kind":"alto","path":"/out/nuc1.20261020T140312123456Z/page.xml"},"markup":"none","run_id":"nuc1.20261020T140312123456Z","status":"ok","text":{"path":"/out/nuc1.20261020T140312123456Z/page.txt"},"type":"result","warnings":[]}"#;

    fn hello() -> Hello {
        match decode(HELLO.as_bytes()).unwrap() {
            FromAdapter::Hello(h) => h,
            other => panic!("{other:?}"),
        }
    }

    fn page() -> PageRequest {
        match decode_request(PAGE.as_bytes()).unwrap() {
            ToAdapter::Page(p) => p,
        }
    }

    fn result() -> PageResult {
        match decode(RESULT.as_bytes()).unwrap() {
            FromAdapter::Result(r) => r,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_spec_examples_round_trip_with_sorted_keys() {
        for line in [HELLO, RESULT] {
            let m = decode(line.as_bytes()).unwrap();
            assert_eq!(String::from_utf8(encode(&m)).unwrap(), format!("{line}\n"));
        }
        let p = decode_request(PAGE.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(encode(&p)).unwrap(), format!("{PAGE}\n"));
        // Unknown fields are ignored within a major version.
        let extra = HELLO.replace("\"cancel\"", "\"new_field\":1,\"cancel\"");
        assert_eq!(
            decode(extra.as_bytes()).unwrap(),
            FromAdapter::Hello(hello())
        );
    }

    #[test]
    fn hello_matches_its_lock_entry_as_sets() {
        let h = hello();
        let mut reordered = h.clone();
        reordered.models.reverse();
        reordered.profiles.reverse();
        reordered.profiles[0].models.reverse();
        assert!(h.matches(&reordered));
        let mut v = h.clone();
        v.engine.version = "5.5.4".into();
        assert!(!h.matches(&v));
        let mut v = h.clone();
        v.profiles[0].languages.push("eng".into());
        assert!(!h.matches(&v), "a profile that gained a language");
        let mut v = h.clone();
        v.models[0].sha256 = "00".into();
        assert!(!h.matches(&v));
        let mut v = h.clone();
        v.cancel = Cancel::Kill;
        assert!(!h.matches(&v));
        let mut v = h.clone();
        v.profiles.push(v.profiles[0].clone());
        assert!(!v.matches(&v), "a duplicate profile id");
        assert!(h.profile("deu").is_some() && h.profile("eng").is_none());
        h.check().unwrap();
        let mut v = h.clone();
        v.contract = 2;
        assert_eq!(v.check(), Err(ProtocolError::Contract(2)));
        let mut v = h.clone();
        v.profiles.push(v.profiles[0].clone());
        assert!(matches!(v.check(), Err(ProtocolError::Hello(_))));
        page().check("/out").unwrap();
        let mut p = page();
        p.deadline_ms = 0;
        assert!(matches!(p.check("/out"), Err(ProtocolError::Page(_))));
        let mut p = page();
        p.out = "/out/other/".into();
        assert!(matches!(p.check("/out"), Err(ProtocolError::Page(_))));
        let mut p = page();
        p.image.dpi = 0;
        assert!(matches!(p.check("/out"), Err(ProtocolError::Page(_))));
    }

    #[test]
    fn results_are_checked_against_the_request() {
        let (h, p, r) = (hello(), page(), result());
        r.check(&p, &h, false).unwrap();
        let mut b = r.clone();
        b.run_id = "other".into();
        assert!(matches!(
            b.check(&p, &h, false),
            Err(ProtocolError::WrongRun { .. })
        ));
        let mut b = r.clone();
        b.layout.as_mut().unwrap().kind = Layout::Boxes;
        assert!(matches!(
            b.check(&p, &h, false),
            Err(ProtocolError::WrongLayout { .. })
        ));
        for bad in [
            "/out/nuc1.20261020T140312123456Z/../x",
            "/out/other/page.xml",
            "/out/nuc1.20261020T140312123456Z/a/b",
            "/etc/passwd",
        ] {
            let mut b = r.clone();
            b.text.as_mut().unwrap().path = bad.into();
            assert!(
                matches!(b.check(&p, &h, false), Err(ProtocolError::Path(_))),
                "{bad}"
            );
        }
        let mut b = r.clone();
        b.layout = None;
        assert_eq!(b.check(&p, &h, false), Err(ProtocolError::NoLayout));
        b.status = Status::Failed;
        assert_eq!(b.check(&p, &h, false), Err(ProtocolError::NoError));
        b.error = Some(ResultError {
            kind: "x".into(),
            message: "m".into(),
        });
        b.check(&p, &h, false).unwrap();
        b.restart = true;
        b.check(&p, &h, false).unwrap();
        b.status = Status::Unsupported;
        assert_eq!(b.check(&p, &h, false), Err(ProtocolError::Restart));
        b.restart = false;
        b.status = Status::Failed;
        b.error = Some(ResultError {
            kind: "x".into(),
            message: "m".repeat(3000),
        });
        assert_eq!(b.check(&p, &h, false), Err(ProtocolError::ErrorTooLong));
        let mut b = r.clone();
        b.engine_stats = (0..65).map(|i| (i.to_string(), 1.into())).collect();
        assert_eq!(b.check(&p, &h, false), Err(ProtocolError::Stats));
        let mut b = r.clone();
        b.partial = true;
        assert_eq!(b.check(&p, &h, false), Err(ProtocolError::Partial));
        b.check(&p, &h, true).unwrap();
        assert!(matches!(
            decode(b"{\"type\":\"page\"}"),
            Err(ProtocolError::NotContract(_))
        ));
        assert!(matches!(
            decode(b"print from a library"),
            Err(ProtocolError::NotContract(_))
        ));
        assert_eq!(
            decode(&vec![b' '; MAX_LINE + 1]),
            Err(ProtocolError::TooLong)
        );
    }

    /// The schemas in `ocr/contract/v1/` are the ones the types generate:
    /// rewrite them with `cargo run -p usnm-ocr-contract --bin
    /// usnm-ocr-contract-schema -- --out-dir ocr/contract/v1`.
    #[test]
    fn checked_in_schemas_match_the_types() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ocr/contract/v1");
        for (name, schema) in schemas() {
            let path = dir.join(format!("{name}.schema.json"));
            let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                on_disk == schema,
                "{} differs from the types; regenerate it with usnm-ocr-contract-schema",
                path.display()
            );
        }
    }
}
