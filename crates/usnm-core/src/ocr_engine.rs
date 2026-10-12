//! `ocr_engine`, the tag that says which engine, version, models and image
//! read a page (13 §13.8.1, #296):
//!
//! ```text
//! <family>/<engine version> profile=<profile> image=sha256:<12 hex> models=sha256:<12 hex> route=<route id>@r<N> contract=<n>
//! ```
//!
//! The worker writes it in every overlay row it makes; the release's engine
//! table and the API use the short key, `<family>/<engine version>
//! <profile>`. `image` and `models` are `local` when the engine didn't run
//! from a pinned image (a developer's host binary); `ocr-sync` refuses rows
//! with `image=local`. The parser also reads the older forms on main,
//! `tesseract-<version>/<models>` and `ndlocr-lite <commit>`, as
//! [`EngineTag::Legacy`].

use std::fmt;

/// A parsed or built `ocr_engine` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineTag {
    Tag(OcrEngineTag),
    /// A value in a form from before this tag (kept as it is).
    Legacy(String),
}

/// The fields of 13 §13.8.1's tag. Built only through [`OcrEngineTag::new`]
/// or [`EngineTag::parse`], which check every field, so a tag always
/// formats to a value that parses back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrEngineTag {
    family: String,
    version: String,
    profile: String,
    image: Digest,
    models: Digest,
    route: String,
    route_version: u32,
    contract: u32,
}

/// The fields of a new tag, for [`OcrEngineTag::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fields<'a> {
    /// `tesseract`.
    pub family: &'a str,
    /// `5.5.0`.
    pub version: &'a str,
    /// The models read together, `frak2021` or `deu_latf+deu`.
    pub profile: &'a str,
    /// The first 12 hex digits of the image's sha256, or `local`.
    pub image: Digest,
    /// The first 12 hex digits of the profile's models' sha256 (sorted
    /// `<name> <sha256>` lines), or `local`.
    pub models: Digest,
    /// The route that picked the profile, and its version (`r<N>`).
    pub route: &'a str,
    pub route_version: u32,
    /// The engine contract's version (0: the worker runs the engine
    /// directly, before ADR-0016's contract).
    pub contract: u32,
}

/// A short sha256 or `local`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Digest {
    Sha256(String),
    Local,
}

/// Why a tag can't be built or read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ocr_engine: {0}")]
pub struct TagError(pub &'static str);

/// `[a-z0-9][a-z0-9_.+-]{0,63}`: family, profile and route ids.
pub fn is_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'+' | b'-')
        })
}

/// `[0-9A-Za-z][0-9A-Za-z.+-]{0,63}`: an engine version.
pub fn is_version(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-'))
}

impl Digest {
    /// The first 12 hex digits of a sha256 given as hex, or as
    /// `sha256:<hex>`.
    pub fn short(sha256: &str) -> Result<Self, TagError> {
        let hex = sha256.strip_prefix("sha256:").unwrap_or(sha256);
        if hex.len() < 12 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(TagError("a digest must be at least 12 hex digits"));
        }
        Ok(Self::Sha256(hex[..12].to_ascii_lowercase()))
    }

    fn parse(s: &str) -> Result<Self, TagError> {
        match s {
            "local" => Ok(Self::Local),
            _ => match s.strip_prefix("sha256:") {
                Some(h) if h.len() == 12 => Self::short(h),
                _ => Err(TagError("a digest is sha256:<12 hex> or local")),
            },
        }
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha256(h) => write!(f, "sha256:{h}"),
            Self::Local => f.write_str("local"),
        }
    }
}

impl OcrEngineTag {
    /// A tag, refusing fields outside 13 §13.8.1's character sets.
    pub fn new(f: Fields<'_>) -> Result<Self, TagError> {
        let tag = Self {
            family: f.family.to_owned(),
            version: f.version.to_owned(),
            profile: f.profile.to_owned(),
            image: f.image,
            models: f.models,
            route: f.route.to_owned(),
            route_version: f.route_version,
            contract: f.contract,
        };
        tag.check()?;
        Ok(tag)
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn image(&self) -> &Digest {
        &self.image
    }

    pub fn models(&self) -> &Digest {
        &self.models
    }

    fn check(&self) -> Result<(), TagError> {
        if !is_id(&self.family) {
            return Err(TagError("bad family"));
        }
        if !is_version(&self.version) {
            return Err(TagError("bad engine version"));
        }
        if !is_id(&self.profile) {
            return Err(TagError("bad profile"));
        }
        if !is_id(&self.route) {
            return Err(TagError("bad route id"));
        }
        Ok(())
    }

    /// `<family>/<engine version> <profile>`.
    pub fn short_key(&self) -> String {
        format!("{}/{} {}", self.family, self.version, self.profile)
    }

    /// Whether the engine ran from a pinned image.
    pub fn pinned(&self) -> bool {
        self.image != Digest::Local
    }
}

impl fmt::Display for OcrEngineTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} profile={} image={} models={} route={}@r{} contract={}",
            self.family,
            self.version,
            self.profile,
            self.image,
            self.models,
            self.route,
            self.route_version,
            self.contract
        )
    }
}

impl EngineTag {
    /// Reads a tag; anything that doesn't start like 13 §13.8.1's form is
    /// [`EngineTag::Legacy`]. A value that starts like it but has a bad
    /// field is an error.
    pub fn parse(s: &str) -> Result<Self, TagError> {
        // The older forms have no `key=value` field after the first.
        if !s.split(' ').skip(1).any(|f| f.contains('=')) {
            return Ok(Self::Legacy(s.to_owned()));
        }
        let mut fields = s.split(' ');
        let head = fields.next().unwrap_or_default();
        let (family, version) = head
            .split_once('/')
            .ok_or(TagError("no <family>/<version>"))?;
        let mut profile = None;
        let mut image = None;
        let mut models = None;
        let mut route = None;
        let mut contract = None;
        for f in fields {
            let (k, v) = f
                .split_once('=')
                .ok_or(TagError("a field isn't key=value"))?;
            let slot = match k {
                "profile" => &mut profile,
                "image" => &mut image,
                "models" => &mut models,
                "route" => &mut route,
                "contract" => &mut contract,
                // `select=` (13 §13.8.1) and later fields are kept out of
                // the struct; they don't change the short key.
                _ => continue,
            };
            if slot.replace(v).is_some() {
                return Err(TagError("a field is repeated"));
            }
        }
        let (route, rv) = route
            .ok_or(TagError("no route"))?
            .split_once("@r")
            .ok_or(TagError("route is <id>@r<N>"))?;
        let tag = OcrEngineTag::new(Fields {
            family,
            version,
            profile: profile.ok_or(TagError("no profile"))?,
            image: Digest::parse(image.ok_or(TagError("no image"))?)?,
            models: Digest::parse(models.ok_or(TagError("no models"))?)?,
            route,
            route_version: rv.parse().map_err(|_| TagError("bad route version"))?,
            contract: contract
                .ok_or(TagError("no contract"))?
                .parse()
                .map_err(|_| TagError("bad contract"))?,
        })?;
        Ok(Self::Tag(tag))
    }

    /// The short key, or a legacy value as it is.
    pub fn short_key(&self) -> String {
        match self {
            Self::Tag(t) => t.short_key(),
            Self::Legacy(s) => s.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Fields<'static> {
        Fields {
            family: "tesseract",
            version: "5.5.0",
            profile: "deu_latf+deu",
            image: Digest::short(
                "sha256:3f9a0c1d2e4b5566778899aabbccddeeff00112233445566778899aabbccddee",
            )
            .unwrap(),
            models: Digest::short("8c2d71aa09fe0000").unwrap(),
            route: "minimal",
            route_version: 1,
            contract: 0,
        }
    }

    fn tag() -> OcrEngineTag {
        OcrEngineTag::new(fields()).unwrap()
    }

    #[test]
    fn formats_and_reads_back() {
        let t = tag();
        let s = t.to_string();
        assert_eq!(
            s,
            "tesseract/5.5.0 profile=deu_latf+deu image=sha256:3f9a0c1d2e4b models=sha256:8c2d71aa09fe route=minimal@r1 contract=0"
        );
        assert_eq!(EngineTag::parse(&s).unwrap(), EngineTag::Tag(t.clone()));
        assert_eq!(t.short_key(), "tesseract/5.5.0 deu_latf+deu");
        assert!(t.pinned());
    }

    #[test]
    fn local_images_read_back_unpinned() {
        let t = OcrEngineTag::new(Fields {
            image: Digest::Local,
            models: Digest::Local,
            ..fields()
        })
        .unwrap();
        let EngineTag::Tag(back) = EngineTag::parse(&t.to_string()).unwrap() else {
            panic!("not a tag");
        };
        assert!(!back.pinned());
        assert!(t.to_string().split(' ').any(|f| f == "image=local"));
    }

    #[test]
    fn older_forms_are_legacy() {
        for s in ["tesseract-5.5.1/deu_latf+deu", "ndlocr-lite 1a2b3c"] {
            let t = EngineTag::parse(s).unwrap();
            assert_eq!(t, EngineTag::Legacy(s.into()));
            assert_eq!(t.short_key(), s);
        }
    }

    #[test]
    fn refuses_bad_fields() {
        for bad in [
            "tesseract/5.5.0 profile=Deu image=local models=local route=minimal@r1 contract=0",
            "tesseract/5.5.0 profile=deu image=sha256:xyz models=local route=minimal@r1 contract=0",
            "tesseract/5.5.0 profile=deu image=local models=local route=minimal contract=0",
            "tesseract/5.5.0 profile=deu image=local models=local route=minimal@r1",
            "tesseract/5.5.0 profile=deu profile=eng image=local models=local route=minimal@r1 contract=0",
            "tesseract/5.5.0 image=local models=local route=minimal@r1 contract=0",
            "tesseract-5.5.0 profile=deu image=local models=local route=minimal@r1 contract=0",
        ] {
            assert!(EngineTag::parse(bad).is_err(), "{bad}");
        }
        for profile in ["deu latf", "Deu", "", "deu=x"] {
            assert!(
                OcrEngineTag::new(Fields {
                    profile,
                    ..fields()
                })
                .is_err(),
                "{profile}"
            );
        }
    }
}
