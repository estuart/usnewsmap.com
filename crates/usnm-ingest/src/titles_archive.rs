//! Title records kept outside an environment's lifetime (#275;
//! docs/operations.md, "Dev environment lifecycle").
//!
//! titles-sync's record cache (`raw/titles.json` in the reference store) and
//! the catalog geocode built from it (`catalog/titles.json`,
//! `catalog/places.json`) live in the environment's own data account, so a
//! dev teardown loses them and the next environment would need another
//! sync paced against LoC (up to 8 hours). With a raw store (`USNM_RAW_URL`,
//! the archival account's `raw` container in dev), titles-sync also saves
//! all three as one create-only copy, `titles/{time}.json`
//! ([`save`]), and a new environment starts from the newest copy
//! ([`seed`], `usnm-ingest titles-seed`, which scripts/stand-up.sh runs).
//!
//! The catalog goes with the records because titles-sync skips titles the
//! catalog already has, and geocode keeps the catalog's ordinals and place
//! ids: the records alone would rebuild a catalog with other numbers.
//!
//! Seeding never replaces anything: it writes only into an environment that
//! has no record cache, each file create-only, the cache last (so a stopped
//! seed runs again). An environment's own cache, newer or older than the
//! copy, stays as it is.

use std::collections::BTreeMap;

use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use usnm_store::ObjectStore;

use crate::catalog::{Place, Title, PLACES, TITLES};
use crate::titles::{RawTitle, RAW};

/// Where the copies go in the raw store: `titles/{time}.json`.
pub const PREFIX: &str = "titles";
/// The time in a copy's name, UTC, as the kept batch lists (`listings/`).
const STAMP: &str = "%Y%m%dT%H%M%SZ";

/// One copy, `titles/{time}.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Copy {
    /// When it was taken.
    pub saved_at: DateTime<Utc>,
    /// The commit of the pipeline that took it (`USNM_GIT_SHA`), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    /// `raw/titles.json` as stored: LoC's records by LCCN.
    pub records: BTreeMap<String, Value>,
    /// `catalog/titles.json` as stored; absent before the first geocode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_titles: Option<Value>,
    /// `catalog/places.json` as stored; absent before the first geocode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_places: Option<Value>,
}

impl Copy {
    /// The same records and catalog, whenever taken.
    fn same_content(&self, other: &Copy) -> bool {
        self.records == other.records
            && self.catalog_titles == other.catalog_titles
            && self.catalog_places == other.catalog_places
    }

    /// Parse each part as the pipeline reads it, so a seed never writes a
    /// file titles-sync or geocode would then fail on.
    fn check(&self) -> anyhow::Result<()> {
        for (lccn, r) in &self.records {
            serde_json::from_value::<RawTitle>(r.clone())
                .with_context(|| format!("record `{lccn}`"))?;
        }
        if let Some(t) = &self.catalog_titles {
            serde_json::from_value::<Vec<Title>>(t.clone()).context(TITLES)?;
        }
        if let Some(p) = &self.catalog_places {
            serde_json::from_value::<Vec<Place>>(p.clone()).context(PLACES)?;
        }
        Ok(())
    }

    fn count(v: &Option<Value>) -> usize {
        v.as_ref().and_then(Value::as_array).map_or(0, Vec::len)
    }
}

/// The time a copy's path names, if it is one (`titles/{time}.json`).
fn stamp(path: &str) -> Option<DateTime<Utc>> {
    let name = path.strip_prefix(PREFIX)?.strip_prefix('/')?;
    let time = name.strip_suffix(".json")?;
    NaiveDateTime::parse_from_str(time, STAMP)
        .ok()
        .map(|t| t.and_utc())
}

/// The newest copy in `raw`, by the time in its name, with its path.
pub async fn newest(raw: &dyn ObjectStore) -> anyhow::Result<Option<(String, Copy)>> {
    let Some(path) = raw
        .list(PREFIX)
        .await?
        .into_iter()
        .filter_map(|p| stamp(&p).map(|t| (t, p)))
        .max()
        .map(|(_, p)| p)
    else {
        return Ok(None);
    };
    let bytes = raw
        .get(&path)
        .await?
        .with_context(|| format!("`{path}` was listed but is gone"))?;
    let copy = serde_json::from_slice(&bytes).with_context(|| path.clone())?;
    Ok(Some((path, copy)))
}

async fn json(store: &dyn ObjectStore, path: &str) -> anyhow::Result<Option<Value>> {
    store
        .get(path)
        .await?
        .map(|b| serde_json::from_slice(&b).with_context(|| path.to_owned()))
        .transpose()
}

/// What [`save`] did.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "saved", rename_all = "snake_case")]
pub enum Saved {
    /// The reference store has no record cache yet.
    Nothing,
    /// The newest copy holds the same records and catalog.
    Unchanged { path: String },
    /// A new copy.
    Written {
        path: String,
        records: usize,
        titles: usize,
        places: usize,
    },
}

/// Copy the record cache and catalog from `reference` to
/// `raw/titles/{now}.json`, unless the newest copy there already holds the
/// same. Create-only: an object already at the path stays as it is.
pub async fn save(
    reference: &dyn ObjectStore,
    raw: &dyn ObjectStore,
    now: DateTime<Utc>,
) -> anyhow::Result<Saved> {
    let records = match json(reference, RAW).await? {
        None => return Ok(Saved::Nothing),
        Some(Value::Object(m)) => m,
        Some(_) => anyhow::bail!("`{RAW}` isn't an object of records by LCCN"),
    };
    let copy = Copy {
        saved_at: now,
        git_sha: std::env::var("USNM_GIT_SHA").ok().filter(|s| !s.is_empty()),
        records: records.into_iter().collect(),
        catalog_titles: json(reference, TITLES).await?,
        catalog_places: json(reference, PLACES).await?,
    };
    if let Some((path, newest)) = newest(raw).await? {
        if newest.same_content(&copy) {
            return Ok(Saved::Unchanged { path });
        }
    }
    let path = format!("{PREFIX}/{}.json", now.format(STAMP));
    if !raw
        .put_new(&path, serde_json::to_vec(&copy)?, "application/json")
        .await?
    {
        // Another run's copy in the same second: that one stays. It counts
        // as this one only if it holds the same; otherwise this run fails,
        // and the next one saves under its own time.
        let bytes = raw
            .get(&path)
            .await?
            .with_context(|| format!("`{path}` was there and is gone"))?;
        let other: Copy = serde_json::from_slice(&bytes).with_context(|| path.clone())?;
        anyhow::ensure!(
            other.same_content(&copy),
            "another run saved a different copy at `{path}` in the same second; not saved, run again"
        );
        return Ok(Saved::Unchanged { path });
    }
    Ok(Saved::Written {
        path,
        records: copy.records.len(),
        titles: Copy::count(&copy.catalog_titles),
        places: Copy::count(&copy.catalog_places),
    })
}

/// What [`seed`] did.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Seeded {
    /// The copy it read, if there is one.
    pub from: Option<String>,
    /// When that copy was taken.
    pub saved_at: Option<DateTime<Utc>>,
    /// The files it wrote to the reference store.
    pub written: Vec<String>,
    /// The files the environment already had, left as they are.
    pub kept: Vec<String>,
    /// Records, catalog titles and places in the copy.
    pub records: usize,
    pub titles: usize,
    pub places: usize,
}

/// Seed `reference` from the newest copy in `raw`, if the environment has
/// no record cache yet. Each file is written create-only, the catalog
/// before the cache: once the cache exists, nothing more is written. A
/// file already there is kept only if it holds what the copy holds (a seed
/// that stopped partway); one that differs stops the seed before the cache
/// is written, so the environment never gets one copy's cache with another
/// catalog.
pub async fn seed(raw: &dyn ObjectStore, reference: &dyn ObjectStore) -> anyhow::Result<Seeded> {
    let mut out = Seeded::default();
    let Some((path, copy)) = newest(raw).await? else {
        return Ok(out);
    };
    copy.check().with_context(|| path.clone())?;
    out.from = Some(path);
    out.saved_at = Some(copy.saved_at);
    out.records = copy.records.len();
    out.titles = Copy::count(&copy.catalog_titles);
    out.places = Copy::count(&copy.catalog_places);
    if reference.exists(RAW).await? {
        out.kept.push(RAW.to_owned());
        return Ok(out);
    }
    let files = [
        (TITLES, copy.catalog_titles.as_ref()),
        (PLACES, copy.catalog_places.as_ref()),
    ];
    for (file, value) in files {
        let Some(value) = value else { continue };
        let bytes = serde_json::to_vec_pretty(value)?;
        if put_new_or_same(reference, file, bytes, value).await? {
            out.written.push(file.to_owned());
        } else {
            out.kept.push(file.to_owned());
        }
    }
    // As titles-sync saves it (compact).
    let records = serde_json::to_value(&copy.records)?;
    if put_new_or_same(reference, RAW, serde_json::to_vec(&copy.records)?, &records).await? {
        out.written.push(RAW.to_owned());
    } else {
        out.kept.push(RAW.to_owned());
    }
    Ok(out)
}

/// Write `bytes` to `path` if nothing is there (true). If something is, it
/// must hold `expected` (false); anything else is an error.
async fn put_new_or_same(
    store: &dyn ObjectStore,
    path: &str,
    bytes: Vec<u8>,
    expected: &Value,
) -> anyhow::Result<bool> {
    if store.put_new(path, bytes, "application/json").await? {
        return Ok(true);
    }
    let existing = json(store, path)
        .await?
        .with_context(|| format!("`{path}` was there and is gone"))?;
    anyhow::ensure!(
        &existing == expected,
        "`{path}` is already there and differs from the copy; not seeding over it (remove it, or let titles-sync build the catalog)"
    );
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use usnm_store::LocalStore;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, h, 0, 0).unwrap()
    }

    fn record(lccn: &str, title: &str) -> Value {
        json!({ "lccn": lccn, "title": title, "cities": ["North Platte"], "states": ["Nebraska"] })
    }

    fn catalog_titles(lccn: &str) -> Value {
        json!([{ "lccn": lccn, "name": "The Tribune", "ordinal": 7, "place_id": "ne-north-platte", "state": "NE" }])
    }

    fn catalog_places() -> Value {
        json!([{ "id": "ne-north-platte", "ordinal": 3, "name": "North Platte", "state": "NE",
                 "lat": 41.12, "lon": -100.77, "precision": "city" }])
    }

    async fn put(store: &LocalStore, path: &str, v: &Value) {
        store
            .put(path, serde_json::to_vec(v).unwrap(), "application/json")
            .await
            .unwrap();
    }

    async fn get(store: &LocalStore, path: &str) -> Option<Value> {
        json(store, path).await.unwrap()
    }

    #[test]
    fn copy_names() {
        assert_eq!(stamp("titles/20261011T120000Z.json"), Some(at(12)));
        assert_eq!(stamp("titles/latest.json"), None);
        assert_eq!(stamp("listings/20261011T120000Z.json"), None);
        assert_eq!(stamp("titles/20261011T120000Z.json.tmp"), None);
    }

    #[tokio::test]
    async fn saves_a_copy_once_per_change() {
        let dir = tempfile::tempdir().unwrap();
        let reference = LocalStore::new(dir.path().join("reference"));
        let raw = LocalStore::new(dir.path().join("raw"));
        assert_eq!(save(&reference, &raw, at(1)).await.unwrap(), Saved::Nothing);

        put(&reference, RAW, &json!({ "sn1": record("sn1", "A") })).await;
        put(&reference, TITLES, &catalog_titles("sn1")).await;
        put(&reference, PLACES, &catalog_places()).await;
        let first = save(&reference, &raw, at(2)).await.unwrap();
        assert_eq!(
            first,
            Saved::Written {
                path: "titles/20261011T020000Z.json".into(),
                records: 1,
                titles: 1,
                places: 1
            }
        );
        // Nothing new: no second copy.
        assert_eq!(
            save(&reference, &raw, at(3)).await.unwrap(),
            Saved::Unchanged {
                path: "titles/20261011T020000Z.json".into()
            }
        );
        put(
            &reference,
            RAW,
            &json!({ "sn1": record("sn1", "A"), "sn2": record("sn2", "B") }),
        )
        .await;
        assert!(matches!(
            save(&reference, &raw, at(4)).await.unwrap(),
            Saved::Written { records: 2, .. }
        ));
        let (path, newest) = newest(&raw).await.unwrap().unwrap();
        assert_eq!(path, "titles/20261011T040000Z.json");
        assert_eq!(newest.saved_at, at(4));
        assert_eq!(newest.records.len(), 2);
        assert_eq!(raw.list(PREFIX).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn seeds_an_empty_environment_from_the_newest_copy() {
        let dir = tempfile::tempdir().unwrap();
        let raw = LocalStore::new(dir.path().join("raw"));
        let reference = LocalStore::new(dir.path().join("reference"));
        assert_eq!(seed(&raw, &reference).await.unwrap(), Seeded::default());

        let older = Copy {
            saved_at: at(1),
            git_sha: None,
            records: [("sn1".to_owned(), record("sn1", "Old"))].into(),
            catalog_titles: None,
            catalog_places: None,
        };
        let newer = Copy {
            saved_at: at(5),
            git_sha: Some("abc".into()),
            records: [
                ("sn1".to_owned(), record("sn1", "A")),
                ("sn2".to_owned(), record("sn2", "B")),
            ]
            .into(),
            catalog_titles: Some(catalog_titles("sn1")),
            catalog_places: Some(catalog_places()),
        };
        put(
            &raw,
            "titles/20261011T010000Z.json",
            &serde_json::to_value(&older).unwrap(),
        )
        .await;
        put(
            &raw,
            "titles/20261011T050000Z.json",
            &serde_json::to_value(&newer).unwrap(),
        )
        .await;
        // Not a copy's name: ignored.
        put(&raw, "titles/notes.json", &json!({})).await;

        let s = seed(&raw, &reference).await.unwrap();
        assert_eq!(s.from.as_deref(), Some("titles/20261011T050000Z.json"));
        assert_eq!(s.saved_at, Some(at(5)));
        assert_eq!(s.written, vec![TITLES, PLACES, RAW]);
        assert!(s.kept.is_empty());
        assert_eq!((s.records, s.titles, s.places), (2, 1, 1));
        let records = get(&reference, RAW).await.unwrap();
        assert_eq!(records["sn2"]["title"], "B");
        assert_eq!(get(&reference, TITLES).await, Some(catalog_titles("sn1")));
        // What titles-sync and geocode read.
        crate::catalog::Catalog::load(&reference).await.unwrap();

        // Again: the environment has its cache now, so nothing is written.
        let again = seed(&raw, &reference).await.unwrap();
        assert!(again.written.is_empty());
        assert_eq!(again.kept, vec![RAW]);
    }

    #[tokio::test]
    async fn never_replaces_an_environments_own_cache() {
        let dir = tempfile::tempdir().unwrap();
        let raw = LocalStore::new(dir.path().join("raw"));
        let reference = LocalStore::new(dir.path().join("reference"));
        let copy = Copy {
            saved_at: at(5),
            git_sha: None,
            records: [("sn1".to_owned(), record("sn1", "From the copy"))].into(),
            catalog_titles: Some(catalog_titles("sn1")),
            catalog_places: Some(catalog_places()),
        };
        put(
            &raw,
            "titles/20261011T050000Z.json",
            &serde_json::to_value(&copy).unwrap(),
        )
        .await;
        let own = json!({ "sn9": record("sn9", "Own") });
        put(&reference, RAW, &own).await;
        let s = seed(&raw, &reference).await.unwrap();
        assert!(s.written.is_empty());
        assert_eq!(s.kept, vec![RAW]);
        assert_eq!(get(&reference, RAW).await, Some(own));
        assert_eq!(get(&reference, TITLES).await, None);

        // A seed that stopped after the catalog's titles: kept, the rest
        // written.
        let dir2 = tempfile::tempdir().unwrap();
        let fresh = LocalStore::new(dir2.path().join("reference"));
        put(&fresh, TITLES, &catalog_titles("sn1")).await;
        let s = seed(&raw, &fresh).await.unwrap();
        assert_eq!(s.written, vec![PLACES, RAW]);
        assert_eq!(s.kept, vec![TITLES]);

        // A catalog of its own without a cache: the seed stops before the
        // cache, so no copy's cache goes with another catalog.
        let dir3 = tempfile::tempdir().unwrap();
        let other = LocalStore::new(dir3.path().join("reference"));
        let own_titles = json!([]);
        put(&other, TITLES, &own_titles).await;
        assert!(seed(&raw, &other).await.is_err());
        assert_eq!(get(&other, TITLES).await, Some(own_titles));
        assert!(!other.exists(RAW).await.unwrap());
    }

    #[tokio::test]
    async fn a_different_copy_in_the_same_second_fails_the_save() {
        let dir = tempfile::tempdir().unwrap();
        let reference = LocalStore::new(dir.path().join("reference"));
        let raw = LocalStore::new(dir.path().join("raw"));
        put(&reference, RAW, &json!({ "sn1": record("sn1", "A") })).await;
        // Another run's copy at the same time, of other records.
        let other = Copy {
            saved_at: at(2),
            git_sha: None,
            records: [("sn2".to_owned(), record("sn2", "B"))].into(),
            catalog_titles: None,
            catalog_places: None,
        };
        // It is also the newest copy, so `save` gets past "same as the
        // newest" and finds the path taken.
        put(
            &raw,
            "titles/20261011T020000Z.json",
            &serde_json::to_value(&other).unwrap(),
        )
        .await;
        assert!(save(&reference, &raw, at(2)).await.is_err());
        // The same records at that time count as saved (here caught as
        // the newest copy already).
        let dir2 = tempfile::tempdir().unwrap();
        let raw2 = LocalStore::new(dir2.path().join("raw"));
        let same = Copy {
            records: [("sn1".to_owned(), record("sn1", "A"))].into(),
            ..other
        };
        put(
            &raw2,
            "titles/20261011T020000Z.json",
            &serde_json::to_value(&same).unwrap(),
        )
        .await;
        assert_eq!(
            save(&reference, &raw2, at(2)).await.unwrap(),
            Saved::Unchanged {
                path: "titles/20261011T020000Z.json".into()
            }
        );
    }

    #[tokio::test]
    async fn refuses_a_copy_the_pipeline_couldnt_read() {
        let dir = tempfile::tempdir().unwrap();
        let raw = LocalStore::new(dir.path().join("raw"));
        let reference = LocalStore::new(dir.path().join("reference"));
        let bad = json!({ "saved_at": "2026-10-11T05:00:00Z",
                          "records": { "sn1": { "lccn": "sn1" } } });
        put(&raw, "titles/20261011T050000Z.json", &bad).await;
        assert!(seed(&raw, &reference).await.is_err());
        assert!(!reference.exists(RAW).await.unwrap());
    }
}
