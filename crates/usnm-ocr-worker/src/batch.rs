//! LoC's batch directories (#368): where a page's original JP2 is, read
//! from the batch's BagIt manifest and the issue's METS, with no loc.gov
//! item API request.
//!
//! Chronicling America's batches are BagIt bags. Each has a
//! `manifest-md5.txt` listing every file with its md5, at paths
//! `data/<lccn>/<reel>/<YYYYMMDD><EE>/<file>`; the files live on
//! `https://tile.loc.gov/storage-services/service/ndnp/<inst>/batch_<batch>_ver<NN>/`
//! (`<inst>` is the batch name's first word, e.g. `mdu`), and
//! `https://chroniclingamerica.loc.gov/data/batches/<batch>_ver<NN>/…`
//! redirects (308) there. A page's JP2 is named by its reel frame, not its
//! sequence number: the issue METS (`<YYYYMMDD><EE>.xml`) maps them, one
//! `np:page` div per page whose MODS `start` is the seq (as Chronicling
//! America's loader reads it) and whose `service` file is `./<frame>.jp2`.
//!
//! The page's IIIF id is the JP2's storage path with `/` as `:`
//! ([`crate::loc::iiif_base`]), the same string the item API route gives.

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use md5::{Digest, Md5};
use quick_xml::events::{BytesStart, Event};
use quick_xml::XmlVersion;

/// An md5 as raw bytes.
pub type Md5Sum = [u8; 16];

/// The md5 of `bytes`, as lowercase hex.
pub fn md5_hex(bytes: &[u8]) -> String {
    hex(&Md5::digest(bytes))
}

/// The md5 of `bytes`.
pub fn md5(bytes: &[u8]) -> Md5Sum {
    Md5::digest(bytes).into()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A 32-character hex md5.
pub fn parse_md5(s: &str) -> Option<Md5Sum> {
    if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// A path segment from LoC's tree that is safe to put in a URL and a file
/// name: letters, digits, `_`, `-` and `.`, not starting with `.`.
pub fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// `<batch>_ver<NN>`, the batch's directory name, if `batch` is a base
/// batch name (lowercase letters, digits and `_`).
pub fn batch_dir(batch: &str, version: u16) -> Option<String> {
    let ok = !batch.is_empty()
        && batch.len() <= 64
        && batch
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !batch.starts_with('_')
        && version > 0;
    ok.then(|| format!("{batch}_ver{version:02}"))
}

/// The issue's directory name: `<YYYYMMDD><EE>`.
pub fn issue_dir(date: NaiveDate, edition: u16) -> String {
    format!("{}{edition:02}", date.format("%Y%m%d"))
}

/// The batch's directory on the storage service, ending in `/`: under
/// `storage_root` (`https://tile.loc.gov/storage-services/service/ndnp/`),
/// `<inst>/batch_<batch dir>/`.
pub fn storage_batch_url(storage_root: &str, batch_dir: &str) -> String {
    let inst = batch_dir.split('_').next().unwrap_or(batch_dir);
    format!("{storage_root}{inst}/batch_{batch_dir}/")
}

/// A file in the bag: `<batch url>data/<lccn>/<reel>/<issue dir>/<file>`.
pub fn file_url(batch_url: &str, lccn: &str, reel: &str, dir: &str, file: &str) -> String {
    format!("{batch_url}data/{lccn}/{reel}/{dir}/{file}")
}

/// The JP2 a IIIF base names, on the storage service: the inverse of
/// [`crate::loc::iiif_base`]. `service_root` is
/// `https://tile.loc.gov/storage-services/service/`.
pub fn jp2_url_of(iiif_root: &str, service_root: &str, iiif_base: &str) -> Option<String> {
    let id = iiif_base
        .strip_prefix(iiif_root)?
        .strip_prefix("service:")?;
    if id.is_empty() || id.contains("..") || !id.split(':').all(safe_segment) {
        return None;
    }
    Some(format!("{service_root}{}.jp2", id.replace(':', "/")))
}

/// One issue's files in a bag.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssueFiles {
    pub reel: String,
    /// `<dir>.xml`, the issue METS, and its md5.
    pub mets: Option<Md5Sum>,
    /// `<dir>_1.xml`, a signed copy some issues have instead or as well.
    pub mets_signed: Option<Md5Sum>,
    /// Frame (`0812`) → the JP2's md5.
    pub jp2: BTreeMap<String, Md5Sum>,
}

impl IssueFiles {
    /// The METS file to read (the plain one first) and its md5.
    pub fn mets_file(&self, dir: &str) -> Option<(String, Md5Sum)> {
        self.mets
            .map(|m| (format!("{dir}.xml"), m))
            .or_else(|| self.mets_signed.map(|m| (format!("{dir}_1.xml"), m)))
    }
}

/// A bag's `manifest-md5.txt`, by issue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BagIndex {
    issues: HashMap<(String, String), IssueFiles>,
    /// JP2s listed.
    pub jp2s: usize,
    /// Issues found under a second reel; the first reel's is kept.
    pub conflicts: usize,
}

impl BagIndex {
    /// Reads `md5  data/<lccn>/<reel>/<dir>/<file>` lines. Lines of any
    /// other shape (tag files, reel-level files, `Thumbs.db`) are skipped.
    pub fn parse(text: &str) -> Self {
        let mut ix = BagIndex::default();
        for line in text.lines() {
            let Some((sum, path)) = line.trim().split_once(char::is_whitespace) else {
                continue;
            };
            let Some(sum) = parse_md5(sum) else { continue };
            let path = path.trim().trim_start_matches('*');
            let segs: Vec<&str> = path.split('/').collect();
            let [data, lccn, reel, dir, file] = segs[..] else {
                continue;
            };
            if data != "data" || ![lccn, reel, dir, file].iter().all(|s| safe_segment(s)) {
                continue;
            }
            let kind = if let Some(stem) = file.strip_suffix(".jp2") {
                Some((Some(stem), false))
            } else if file.strip_suffix(".xml") == Some(dir) {
                Some((None, false))
            } else if file.strip_suffix("_1.xml") == Some(dir) {
                Some((None, true))
            } else {
                None
            };
            let Some((stem, signed)) = kind else { continue };
            let e = ix
                .issues
                .entry((lccn.to_owned(), dir.to_owned()))
                .or_insert_with(|| IssueFiles {
                    reel: reel.to_owned(),
                    ..IssueFiles::default()
                });
            if e.reel != reel {
                ix.conflicts += 1;
                continue;
            }
            match (stem, signed) {
                (Some(stem), _) => {
                    if e.jp2.insert(stem.to_owned(), sum).is_none() {
                        ix.jp2s += 1;
                    }
                }
                (None, false) => e.mets = Some(sum),
                (None, true) => e.mets_signed = Some(sum),
            }
        }
        ix
    }

    pub fn issue(&self, lccn: &str, dir: &str) -> Option<&IssueFiles> {
        self.issues.get(&(lccn.to_owned(), dir.to_owned()))
    }

    pub fn issues(&self) -> usize {
        self.issues.len()
    }
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.local_name().as_ref() == name)
            .then(|| {
                a.normalized_value(XmlVersion::Implicit1_0)
                    .ok()
                    .map(|v| v.into_owned())
            })
            .flatten()
    })
}

/// Seq → frame (`0812`) from an issue METS: each `np:page` div in the
/// structMap, its seq the MODS `start` of the dmdSec it names, its frame
/// the JP2 among its files (the `service` file first). Pages with no
/// numeric seq or no JP2 are left out; a seq given twice is an error.
pub fn mets_pages(xml: &[u8]) -> Result<BTreeMap<u16, String>, String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    // dmdSec ID → MODS start.
    let mut starts: HashMap<String, String> = HashMap::new();
    // file ID → (USE, href).
    let mut files: HashMap<String, (String, String)> = HashMap::new();
    // (DMDID, FILEIDs) in structMap order.
    let mut pages: Vec<(String, Vec<String>)> = Vec::new();
    let mut dmd: Option<String> = None;
    let mut in_start = false;
    let mut file: Option<(String, String)> = None;
    let mut div_depth = 0usize;
    let mut page: Option<(usize, String, Vec<String>)> = None;
    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("METS doesn't parse: {e}"))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                match e.local_name().as_ref() {
                    "dmdSec" if !empty => dmd = attr(e, "ID"),
                    "start" if !empty && dmd.is_some() => in_start = true,
                    "file" if !empty => {
                        file = attr(e, "ID").map(|id| (id, attr(e, "USE").unwrap_or_default()))
                    }
                    "FLocat" => {
                        if let (Some((id, usage)), Some(href)) = (&file, attr(e, "href")) {
                            files
                                .entry(id.clone())
                                .or_insert_with(|| (usage.clone(), href));
                        }
                    }
                    "div" if !empty => {
                        div_depth += 1;
                        if page.is_none() && attr(e, "TYPE").as_deref() == Some("np:page") {
                            page = Some((div_depth, attr(e, "DMDID").unwrap_or_default(), vec![]));
                        }
                    }
                    "fptr" => {
                        if let (Some((_, _, ids)), Some(id)) = (&mut page, attr(e, "FILEID")) {
                            ids.push(id);
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) if in_start => {
                if let Some(id) = &dmd {
                    let v = t.xml10_content().trim().to_owned();
                    starts.entry(id.clone()).or_insert(v);
                }
            }
            Event::End(ref e) => match e.local_name().as_ref() {
                "dmdSec" => dmd = None,
                "start" => in_start = false,
                "file" => file = None,
                "div" => {
                    if page.as_ref().is_some_and(|(d, _, _)| *d == div_depth) {
                        let (_, id, ids) = page.take().expect("checked");
                        pages.push((id, ids));
                    }
                    div_depth = div_depth.saturating_sub(1);
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let mut out = BTreeMap::new();
    for (dmdid, ids) in pages {
        let Some(seq) = starts.get(&dmdid).and_then(|s| s.parse::<u16>().ok()) else {
            continue;
        };
        let jp2 = |want_service: bool| {
            ids.iter().find_map(|id| {
                let (usage, href) = files.get(id)?;
                if want_service && usage != "service" {
                    return None;
                }
                let stem = href
                    .strip_prefix("./")
                    .unwrap_or(href)
                    .strip_suffix(".jp2")?;
                safe_segment(stem).then(|| stem.to_owned())
            })
        };
        let Some(frame) = jp2(true).or_else(|| jp2(false)) else {
            continue;
        };
        if out.insert(seq, frame).is_some() {
            return Err(format!("METS gives seq {seq} twice"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The issue METS of the Deutsche Correspondent of 29 July 1904
    /// (`mdu_garlic_ver02`), cut to three of its eight pages, with page 7's
    /// files listed before page 6's so file order isn't seq order.
    pub const METS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mets xmlns:MODS="http://www.loc.gov/mods/v3" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns="http://www.loc.gov/METS/">
  <dmdSec ID="issueModsBib"><mdWrap MDTYPE="MODS"><xmlData><MODS:mods>
    <MODS:relatedItem type="host"><MODS:identifier type="lccn">sn83045081</MODS:identifier></MODS:relatedItem>
  </MODS:mods></xmlData></mdWrap></dmdSec>
  <dmdSec ID="pageModsBib1"><mdWrap MDTYPE="MODS"><xmlData><MODS:mods>
    <MODS:part><MODS:extent unit="pages"><MODS:start>1</MODS:start></MODS:extent></MODS:part>
    <MODS:relatedItem type="original"><MODS:identifier type="reel sequence number">806</MODS:identifier></MODS:relatedItem>
  </MODS:mods></xmlData></mdWrap></dmdSec>
  <dmdSec ID="pageModsBib6"><mdWrap MDTYPE="MODS"><xmlData><MODS:mods>
    <MODS:part><MODS:extent unit="pages"><MODS:start>6</MODS:start></MODS:extent></MODS:part>
  </MODS:mods></xmlData></mdWrap></dmdSec>
  <dmdSec ID="pageModsBib7"><mdWrap MDTYPE="MODS"><xmlData><MODS:mods>
    <MODS:part><MODS:extent unit="pages"><MODS:start>7</MODS:start></MODS:extent></MODS:part>
  </MODS:mods></xmlData></mdWrap></dmdSec>
  <fileSec>
    <fileGrp ID="pageFileGrp1">
      <file ID="masterFile1" USE="master"><FLocat LOCTYPE="OTHER" OTHERLOCTYPE="file" xlink:href="./0806.tif"/></file>
      <file ID="serviceFile1" USE="service"><FLocat LOCTYPE="OTHER" OTHERLOCTYPE="file" xlink:href="./0806.jp2"/></file>
      <file ID="ocrFile1" USE="ocr"><FLocat LOCTYPE="OTHER" OTHERLOCTYPE="file" xlink:href="./0806.xml"/></file>
    </fileGrp>
    <fileGrp ID="pageFileGrp7">
      <file ID="masterFile7" USE="master"><FLocat LOCTYPE="OTHER" OTHERLOCTYPE="file" xlink:href="./0812.tif"/></file>
      <file ID="serviceFile7" USE="service"><FLocat LOCTYPE="OTHER" OTHERLOCTYPE="file" xlink:href="./0812.jp2"/></file>
    </fileGrp>
    <fileGrp ID="pageFileGrp6">
      <file ID="serviceFile6" USE="service"><FLocat LOCTYPE="OTHER" OTHERLOCTYPE="file" xlink:href="./0811.jp2"/></file>
    </fileGrp>
  </fileSec>
  <structMap xmlns:np="urn:library-of-congress:ndnp:mets:newspaper">
    <div TYPE="np:issue" DMDID="issueModsBib">
      <div TYPE="np:page" DMDID="pageModsBib1"><fptr FILEID="masterFile1"/><fptr FILEID="serviceFile1"/><fptr FILEID="ocrFile1"/></div>
      <div TYPE="np:page" DMDID="pageModsBib6"><fptr FILEID="serviceFile6"/></div>
      <div TYPE="np:page" DMDID="pageModsBib7"><fptr FILEID="masterFile7"/><fptr FILEID="serviceFile7"/></div>
    </div>
  </structMap>
</mets>"#;

    /// Lines of `mdu_garlic_ver02`'s `manifest-md5.txt`, md5s as LoC lists them.
    pub const BAG: &str = "\
92d9a0cec5176543bd392c63a72195c5  data/sn83045081/0029602241A/1904072901/1904072901.xml
2bf3a0263e60ced7b9b7e6c983ed6ccf  data/sn83045081/0029602241A/1904072901/1904072901_1.xml
0006ea39cc07715ae2bfdd9143179867  data/sn83045081/0029602241A/1904072901/0812.jp2
95d8f305355a480dbc02d501bba07729  data/sn83045081/0029602241A/1904072901/0813.pdf
5a4d4dffaabff105aed16bcfc9b6ad27  data/sn83045081/0029602241A/1904072901/0811.jp2
a3a00bb3781515f00403f80f0fe513a4  data/sn83045081/0029602241A/1904072901/0806.jp2
c0ea1892567b6ba4c82ad30923df67aa  data/sn83045081/0029602241A/1904072901/0811.xml
a640168aa4004e61e4506ea464bf3638  data/sn83045081/0029602241A/1904052601/1904052601_1.xml
3805180c8b4b8b79c377fe0b2d1eaffe  data/sn83045081/00296022548/1902020201/Thumbs.db
0db5606b40538adf4f25c208916e2987  data/sn83045081/00296022408/Thumbs.db
ffffffffffffffffffffffffffffffff  data/sn83045081/9999/1904072901/0001.jp2
nothex  data/sn1/1/1890050301/0001.jp2
00000000000000000000000000000000  data/sn1/../1890050301/0001.jp2
";

    #[test]
    fn maps_seq_to_frame_from_the_mets() {
        let pages = mets_pages(METS.as_bytes()).unwrap();
        assert_eq!(
            pages,
            BTreeMap::from([
                (1, "0806".to_owned()),
                (6, "0811".to_owned()),
                (7, "0812".to_owned())
            ])
        );
        // The same seq twice can't be resolved.
        let twice = METS.replace("<MODS:start>6</MODS:start>", "<MODS:start>7</MODS:start>");
        assert!(mets_pages(twice.as_bytes()).is_err());
        // A page with no numeric seq is left out.
        let roman = METS.replace("<MODS:start>6</MODS:start>", "<MODS:start>vi</MODS:start>");
        assert!(!mets_pages(roman.as_bytes()).unwrap().contains_key(&6));
        assert!(mets_pages(b"<mets><dmdSec").is_err());
    }

    #[test]
    fn indexes_the_bag_manifest_by_issue() {
        let ix = BagIndex::parse(BAG);
        let issue = ix.issue("sn83045081", "1904072901").unwrap();
        assert_eq!(issue.reel, "0029602241A");
        assert_eq!(
            issue.jp2.keys().collect::<Vec<_>>(),
            ["0806", "0811", "0812"]
        );
        assert_eq!(hex(&issue.jp2["0812"]), "0006ea39cc07715ae2bfdd9143179867");
        assert_eq!(
            issue.mets_file("1904072901").unwrap(),
            (
                "1904072901.xml".to_owned(),
                parse_md5("92d9a0cec5176543bd392c63a72195c5").unwrap()
            )
        );
        // Only the signed METS: that one.
        let signed = ix.issue("sn83045081", "1904052601").unwrap();
        assert_eq!(
            signed.mets_file("1904052601").unwrap().0,
            "1904052601_1.xml"
        );
        // The second reel's copy, the bad md5 and the path out of the tree
        // aren't taken.
        assert_eq!((ix.jp2s, ix.conflicts, ix.issues()), (3, 1, 2));
        assert!(ix.issue("sn1", "1890050301").is_none());
    }

    #[test]
    fn builds_urls_and_the_iiif_id_from_the_batch_path() {
        let dir = batch_dir("mdu_garlic", 2).unwrap();
        assert_eq!(dir, "mdu_garlic_ver02");
        assert_eq!(batch_dir("Bad-Name", 1), None);
        assert_eq!(batch_dir("mdu_garlic", 0), None);
        let d = issue_dir(NaiveDate::from_ymd_opt(1904, 7, 29).unwrap(), 1);
        assert_eq!(d, "1904072901");
        let base = storage_batch_url("https://tile.loc.gov/storage-services/service/ndnp/", &dir);
        assert_eq!(
            base,
            "https://tile.loc.gov/storage-services/service/ndnp/mdu/batch_mdu_garlic_ver02/"
        );
        let jp2 = file_url(&base, "sn83045081", "0029602241A", &d, "0812.jp2");
        assert_eq!(
            jp2,
            "https://tile.loc.gov/storage-services/service/ndnp/mdu/batch_mdu_garlic_ver02/data/sn83045081/0029602241A/1904072901/0812.jp2"
        );
        // The IIIF id from the batch path is the one the item API route gave
        // for this page (its sidecar on pve-big).
        let iiif =
            crate::loc::iiif_base("https://tile.loc.gov/image-services/iiif/", &jp2).unwrap();
        assert_eq!(
            iiif,
            "https://tile.loc.gov/image-services/iiif/service:ndnp:mdu:batch_mdu_garlic_ver02:data:sn83045081:0029602241A:1904072901:0812"
        );
        // And back.
        assert_eq!(
            jp2_url_of(
                "https://tile.loc.gov/image-services/iiif/",
                "https://tile.loc.gov/storage-services/service/",
                &iiif
            )
            .unwrap(),
            jp2
        );
        assert_eq!(
            jp2_url_of("r/", "s/", "r/service:a:..:b"),
            None,
            "no way out of the tree"
        );
    }

    #[test]
    fn md5s() {
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex(&md5(b"abc")), md5_hex(b"abc"));
        assert_eq!(
            parse_md5("900150983cd24fb0d6963f7d28e17f72"),
            Some(md5(b"abc"))
        );
        assert_eq!(parse_md5("90015"), None);
        assert_eq!(parse_md5("zz0150983cd24fb0d6963f7d28e17f72"), None);
    }
}
