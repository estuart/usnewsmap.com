//! A dry run of `geocode`'s place rules (04 §4.6) against a published
//! `/v1/places` GeoJSON, without LoC's title records:
//!
//!     curl -s https://usnewsmap.com/v1/places > places.json
//!     cargo run -p usnm-ingest --example place-report -- --places places.json > report.txt
//!
//! Each live place becomes that many stand-in titles at its live point. The
//! records behind a point aren't in the GeoJSON, so a point two places with
//! different keys share in one state (the mark of a paper that moved: one
//! latlong for every city its record lists) counts as coming from
//! multi-city records, and any other as LoC's own point for the city. The
//! report lists the merges, renamed places, places that move, and every
//! place's distance from the gazetteer. It can't list places whose LoC
//! points are far apart (`loc_apart`): that needs the records.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;
use usnm_core::names::STATES;
use usnm_ingest::catalog::{Place, Title};
use usnm_ingest::places::{self, distance_km, Geo};
use usnm_ingest::titles::{build, place_overrides, RawTitle, PLACE_OVERRIDES};

struct Live {
    id: String,
    ordinal: u32,
    name: String,
    state: String,
    titles: usize,
    lat: f64,
    lon: f64,
    precision: String,
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    let mut path = None;
    // The positional form (`place-report FILE`) works for one release, with
    // a warning (#277).
    if let Some(p) = args.next_if(|a| !a.starts_with('-')) {
        eprintln!("warning: `place-report FILE` is deprecated; use --places FILE (#277)");
        path = Some(p);
    }
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--places", Some(p)) => path = Some(p),
            _ => anyhow::bail!("usage: place-report [--places FILE] (default /tmp/places.json)"),
        }
    }
    let path = path.unwrap_or_else(|| "/tmp/places.json".into());
    let json: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let geo = Geo::compiled()?;
    let overrides = place_overrides(PLACE_OVERRIDES)?;
    let mut live = Vec::new();
    for f in json["features"].as_array().into_iter().flatten() {
        let p = &f["properties"];
        let id = f["id"].as_str().unwrap_or_default().to_owned();
        live.push(Live {
            ordinal: id.trim_start_matches('P').parse()?,
            id,
            name: p["name"].as_str().unwrap_or_default().to_owned(),
            state: p["state"].as_str().unwrap_or_default().to_owned(),
            titles: p["titles"].as_u64().unwrap_or(1) as usize,
            lon: f["geometry"]["coordinates"][0].as_f64().unwrap_or_default(),
            lat: f["geometry"]["coordinates"][1].as_f64().unwrap_or_default(),
            precision: p["precision"].as_str().unwrap_or("city").to_owned(),
        });
    }
    let state_of = |code: &str| STATES.iter().find(|s| s.code == code);
    let key_of = |l: &Live| {
        state_of(&l.state)
            .map(|s| geo.city(&l.name, s).key)
            .unwrap_or_default()
    };

    // Points shared by places with different keys in one state.
    let mut at: HashMap<(String, String, String), Vec<String>> = HashMap::new();
    for l in &live {
        at.entry((
            l.state.clone(),
            format!("{:.5}", l.lat),
            format!("{:.5}", l.lon),
        ))
        .or_default()
        .push(key_of(l));
    }
    let shared = |l: &Live| {
        at[&(
            l.state.clone(),
            format!("{:.5}", l.lat),
            format!("{:.5}", l.lon),
        )]
            .iter()
            .any(|k| *k != key_of(l))
    };

    let mut raw = BTreeMap::new();
    let mut titles = Vec::new();
    let mut places = Vec::new();
    let mut ordinal = 0;
    for l in &live {
        let Some(state) = state_of(&l.state) else {
            continue;
        };
        places.push(Place {
            id: l.id.clone(),
            ordinal: l.ordinal,
            name: l.name.clone(),
            state: l.state.clone(),
            lat: l.lat,
            lon: l.lon,
            precision: l.precision.clone(),
            coordinates_from: None,
            variants: vec![],
        });
        let multi = shared(l);
        for k in 0..l.titles {
            ordinal += 1;
            let lccn = format!("sn{:05}{k:03}", l.ordinal);
            let mut cities = vec![l.name.to_lowercase()];
            if multi {
                cities.push("somewhere else".into());
            }
            raw.insert(
                lccn.clone(),
                RawTitle {
                    lccn: lccn.clone(),
                    title: format!("Paper {k} ({})", l.name),
                    cities,
                    states: vec![state.name.to_lowercase()],
                    latlong: (l.precision == "city").then_some([l.lat, l.lon]),
                    dates: None,
                    languages: vec![],
                    record: None,
                },
            );
            titles.push(Title {
                lccn,
                name: format!("Paper {k}"),
                ordinal,
                place_id: l.id.clone(),
                state: l.state.clone(),
                languages: vec![],
                extra: BTreeMap::new(),
            });
        }
    }
    let (catalog, report) = build(&raw, titles, places, &overrides, geo)?;

    let mut new_place_of: HashMap<&str, &str> = HashMap::new();
    for l in &live {
        let lccn = format!("sn{:05}000", l.ordinal);
        if let Some(t) = catalog.title(&lccn) {
            new_place_of.insert(&l.id, &t.place_id);
        }
    }
    let after: std::collections::BTreeSet<&str> = new_place_of.values().copied().collect();

    println!("Place rules dry run on {path}");
    println!("(stand-in titles at the live points; see the example's header)\n");
    println!("places before: {}", live.len());
    println!("places after:  {}", after.len());
    println!(
        "coordinates: override {}, LoC single-city {}, gazetteer {}, gazetteer over a LoC point > 100 km off {}, LoC multi-city {}, state centroid {}",
        report.from_override,
        report.from_loc,
        report.from_gazetteer,
        report.gazetteer_over_loc,
        report.from_loc_multi_city,
        report.state_centroid
    );
    println!(
        "titles renamed by an alias: {}; places retired: {}",
        report.aliased,
        report.retired.len()
    );
    println!(
        "LoC single-city points more than 25 km from the gazetteer (kept, for review): {}\n",
        report.disagreements
    );

    // Merges.
    let mut merged: BTreeMap<&str, Vec<&Live>> = BTreeMap::new();
    for l in &live {
        if let Some(p) = new_place_of.get(l.id.as_str()) {
            merged.entry(p).or_default().push(l);
        }
    }
    let groups: Vec<_> = merged.iter().filter(|(_, v)| v.len() > 1).collect();
    println!(
        "== Merges: {} groups, {} live places into {} ==",
        groups.len(),
        groups.iter().map(|(_, v)| v.len()).sum::<usize>(),
        groups.len()
    );
    for (id, members) in &groups {
        let p = catalog.place(id).expect("place");
        let parts: Vec<String> = members
            .iter()
            .map(|l| format!("{} {} ({})", l.id, l.name, l.titles))
            .collect();
        println!("{} {}, {}: {}", p.id, p.name, p.state, parts.join(" + "));
    }

    // Renamed.
    println!("\n== Renamed (same id, new display name) ==");
    let mut renamed = 0;
    for l in &live {
        if let Some(p) = catalog.place(&l.id) {
            if after.contains(l.id.as_str()) && p.name != l.name {
                println!("{} {}: {} → {}", l.id, l.state, l.name, p.name);
                renamed += 1;
            }
        }
    }
    println!("({renamed})");

    // Moves.
    let mut moves: Vec<(f64, &Live, &Place)> = live
        .iter()
        .filter_map(|l| {
            let p = catalog.place(new_place_of.get(l.id.as_str())?)?;
            Some((distance_km((l.lat, l.lon), (p.lat, p.lon)), l, p))
        })
        .filter(|(km, _, _)| *km > 25.0)
        .collect();
    moves.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!(
        "\n== Live places whose point moves more than 25 km: {} ==",
        moves.len()
    );
    for (km, l, p) in &moves {
        println!(
            "{:>6.0} km  {} {}, {} ({},{}) → {} {} ({:.4},{:.4})",
            km, l.id, l.name, l.state, l.lat, l.lon, p.id, p.name, p.lat, p.lon
        );
    }

    println!(
        "\n== LoC points replaced by the gazetteer (> 100 km from the gazetteer's town, name not ambiguous): {} ==",
        report.gazetteer_over_loc
    );
    for d in &report.gazetteer_over_loc_places {
        println!("{d}");
    }

    println!(
        "\n== Disagreements kept for review (furthest {}) ==",
        report.disagreement_examples.len()
    );
    for d in &report.disagreement_examples {
        println!("{d}");
    }

    // The stand-in titles all sit at their place's live point, under
    // made-up LCCNs, so `loc_apart` would be empty or list points that
    // aren't LoC's: it needs LoC's title records (`geocode` logs it).
    println!(
        "\n== Places at the median of LoC points more than 25 km apart ==\n\
         Not available from /v1/places: it needs LoC's title records (raw/titles.json). \
         The geocode log lists them (loc_apart_examples)."
    );

    // Every live place against the gazetteer.
    println!("\n== Every live place: gazetteer point and distance ==");
    println!("id\tstate\tname\ttitles\tlive\tgazetteer\tkm\tshared point");
    for l in &live {
        let gaz = geo.gazetteer.near(&l.state, places::plain_key(&key_of(l)));
        let (g, km) = match gaz {
            Some(g) => (
                format!("{} ({:.4},{:.4})", g.name, g.lat, g.lon),
                format!("{:.0}", distance_km((l.lat, l.lon), (g.lat, g.lon))),
            ),
            None => ("-".into(), "-".into()),
        };
        println!(
            "{}\t{}\t{}\t{}\t{:.4},{:.4}\t{}\t{}\t{}",
            l.id,
            l.state,
            l.name,
            l.titles,
            l.lat,
            l.lon,
            g,
            km,
            if shared(l) { "yes" } else { "" }
        );
    }
    Ok(())
}
