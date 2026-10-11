//! Reading a Congressional Globe page (#229 design §5, §6): its text from
//! UNT's word annotations, and from that text its day and chamber headings,
//! where the appendix starts, its printed page number and the members it
//! names.
//!
//! UNT's OCR comes as one IIIF annotation per word, in reading order (column
//! by column), with the word's box. Most punctuation is gone from the words
//! (`Mr CLAY of Alabama`, `THURSDAY March 29 1838`), so the rules here read
//! tokens, not punctuation. Every rule errs towards missing a heading rather
//! than taking a line of debate for one: a missed day heading leaves the
//! page dated by the one before it, which is usually the same day.

use chrono::{Datelike, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

/// One word annotation (`sc:AnnotationList` `resources[]`).
#[derive(Debug, Deserialize)]
struct Annotation {
    #[serde(default)]
    resource: Option<Content>,
    #[serde(default)]
    on: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Content {
    #[serde(default)]
    chars: String,
}

/// An OCR annotation list. `resources` is required (an empty list is a page
/// with no text): a response without it, such as `{}`, isn't an annotation
/// list, and is refused rather than kept as a textless page.
#[derive(Debug, Deserialize)]
struct AnnotationList {
    resources: Vec<Annotation>,
}

/// A word's box on the canvas, from `on`'s `#xywh=x,y,w,h`.
fn xywh(on: &str) -> Option<(i64, i64, i64, i64)> {
    let (_, frag) = on.rsplit_once("#xywh=")?;
    let mut it = frag.split(',').map(|n| n.trim().parse::<i64>().ok());
    Some((it.next()??, it.next()??, it.next()??, it.next()??))
}

/// The page's lines from UNT's OCR annotation list, in reading order. A word
/// starts a new line when it sits lower or higher than the last by more than
/// half its height, or left of it; a word above the last line's top starts a
/// new column, marked by an empty line. Words without a box are appended to
/// the current line.
pub fn page_lines(json: &[u8]) -> anyhow::Result<Vec<String>> {
    let list: AnnotationList = serde_json::from_slice(json)?;
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut last: Option<(i64, i64)> = None;
    for a in list.resources {
        let Some(word) = a.resource.map(|r| r.chars) else {
            continue;
        };
        let word = word.trim();
        if let Some((x, y, _, h)) = a.on.as_deref().and_then(xywh) {
            if let Some((px, py)) = last {
                if (y - py).abs() * 2 > h.max(1) || x < px {
                    lines.push(std::mem::take(&mut line));
                    if y + h < py {
                        lines.push(String::new());
                    }
                }
            }
            last = Some((x, y));
        }
        if word.is_empty() {
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    // Leading and trailing blank lines, and runs of them, carry nothing.
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for l in lines {
        let l = l.trim().to_owned();
        if l.is_empty() && out.last().is_none_or(String::is_empty) {
            continue;
        }
        out.push(l);
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    Ok(out)
}

/// A chamber, as a heading names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Chamber {
    Senate,
    House,
}

impl Chamber {
    pub fn as_str(self) -> &'static str {
        match self {
            Chamber::Senate => "senate",
            Chamber::House => "house",
        }
    }
}

/// What one line says, when it is a heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heading {
    /// A day: `THURSDAY, March 29, 1838` (a day heading), or `In Senate,
    /// March 3, 1838` (an appendix speech's).
    pub date: Option<NaiveDate>,
    pub chamber: Option<Chamber>,
}

const WEEKDAYS: [(&str, Weekday); 7] = [
    ("MONDAY", Weekday::Mon),
    ("TUESDAY", Weekday::Tue),
    ("WEDNESDAY", Weekday::Wed),
    ("THURSDAY", Weekday::Thu),
    ("FRIDAY", Weekday::Fri),
    ("SATURDAY", Weekday::Sat),
    ("SUNDAY", Weekday::Sun),
];

const MONTHS: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];

/// The years a heading may name: the debates' (1789 to 1873), with a year's
/// slack at the end for a volume's front matter.
const YEARS: std::ops::RangeInclusive<i32> = 1789..=1875;

/// Levenshtein distance, for OCR-damaged heading words (`TftuasDAY`).
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// A token without the punctuation OCR leaves around it.
fn clean(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_alphanumeric())
}

/// How a token reads as a weekday: `Some(Some(day))` spelled exactly,
/// `Some(None)` near enough (`TftuasDAY`, `Thursdav`), `None` not one.
/// Near enough is one wrong letter from a weekday of up to 7 letters, two
/// from a longer one, or, for a token of 8 letters or more that ends in
/// `DAY`, three from a weekday of 8 or more: so damaged headings pass, and
/// words such as `YESTERDAY`, `HOLIDAY`, `MIDDAY` or `WEEKDAY` don't.
fn weekday(token: &str) -> Option<Option<Weekday>> {
    let t = token.to_uppercase();
    if let Some((_, w)) = WEEKDAYS.iter().find(|(n, _)| *n == t) {
        return Some(Some(*w));
    }
    let n = t.chars().count();
    let near = (6..=10).contains(&n)
        && WEEKDAYS.iter().any(|(w, _)| {
            let len = w.chars().count();
            let d = distance(&t, w);
            d <= if len <= 7 { 1 } else { 2 }
                || (t.ends_with("DAY") && n >= 8 && len >= 8 && d <= 3)
        });
    near.then_some(None)
}

/// A month by its name, allowing one wrong letter in the long ones.
fn month(token: &str) -> Option<u32> {
    let t = token.to_uppercase();
    MONTHS
        .iter()
        .position(|m| *m == t || (m.len() >= 5 && t.len() >= 5 && distance(&t, m) <= 1))
        .map(|i| i as u32 + 1)
}

/// The words of a chamber heading, `0`/`P`/`Q` misread `O`/`F` included.
fn chamber_words(tokens: &[String]) -> Option<Chamber> {
    let words: Vec<String> = tokens
        .iter()
        .map(|t| t.to_uppercase().replace('0', "O"))
        .filter(|t| t != "IN" && t != "THE")
        .collect();
    match words.as_slice() {
        [s] if s == "SENATE" => Some(Chamber::Senate),
        [h, of, r]
            if h == "HOUSE"
                && matches!(of.as_str(), "OF" | "OP" | "QF" | "OE")
                && r.starts_with("REPRESENT") =>
        {
            Some(Chamber::House)
        }
        _ => None,
    }
}

/// Whether a line is in capitals: every letter in it is upper case.
fn upper(line: &str) -> bool {
    line.chars().any(char::is_alphabetic)
        && line
            .chars()
            .filter(|c| c.is_alphabetic())
            .all(char::is_uppercase)
}

/// The heading a line is, if it is one:
///
/// - a day heading: a weekday, then month, day and year, and nothing else
///   (`THURSDAY, March 29, 1838`). A weekday spelled out in full must be
///   that date's weekday;
/// - a chamber heading, in capitals: `SENATE`, `IN SENATE`, `HOUSE OF
///   REPRESENTATIVES`, `IN THE HOUSE OF REPRESENTATIVES`;
/// - both: a chamber, then the date (`In Senate, March 3, 1838`), as the
///   appendix heads its speeches.
///
/// An issue's masthead (`25th Cong. 2nd Sess. MONDAY APRIL 9 1838 Volume 6
/// No 18`) is not a heading: its date is the issue's, and the debates under
/// it carry on from the last issue (`Continued from No 17`) until a day
/// heading says otherwise.
pub fn heading(line: &str) -> Option<Heading> {
    let tokens: Vec<String> = line
        .split_whitespace()
        .map(clean)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect();
    if tokens.is_empty() || tokens.len() > 9 {
        return None;
    }
    // Month, day and year end the line.
    if tokens.len() >= 4 {
        let n = tokens.len();
        let (m, d, y) = (&tokens[n - 3], &tokens[n - 2], &tokens[n - 1]);
        let date = month(m).and_then(|m| {
            let d: u32 = d.parse().ok().filter(|_| d.len() <= 2)?;
            let y: i32 = y.parse().ok().filter(|_| y.len() == 4)?;
            YEARS
                .contains(&y)
                .then(|| NaiveDate::from_ymd_opt(y, m, d))
                .flatten()
        });
        if let Some(date) = date {
            let before = &tokens[..n - 3];
            return match before {
                [] => None,
                [w] => match weekday(w)? {
                    Some(named) if named != date.weekday() => None,
                    _ => Some(Heading {
                        date: Some(date),
                        chamber: None,
                    }),
                },
                _ => {
                    // A chamber, then perhaps the weekday.
                    let (words, last) = before.split_at(before.len() - 1);
                    let (chamber, ok_weekday) = match weekday(&last[0]) {
                        Some(named) => (
                            chamber_words(words),
                            named.is_none_or(|w| w == date.weekday()),
                        ),
                        None => (chamber_words(before), true),
                    };
                    (chamber.is_some() && ok_weekday).then_some(Heading {
                        date: Some(date),
                        chamber,
                    })
                }
            };
        }
    }
    (upper(line) && tokens.len() <= 5)
        .then(|| chamber_words(&tokens))
        .flatten()
        .map(|c| Heading {
            date: None,
            chamber: Some(c),
        })
}

/// Whether a page starts the appendix (or is one of its pages): one of its
/// first four lines, alone or run into the next, starts `APPENDIX TO THE
/// CONG…`, after a page number at most (`APPENDIX` / `TO THE
/// CONG1BSSIOÍÍAL GLOBE` on the title page, `APPENDIX TO THE CONGRESSIONAL
/// GLOBE` in the running head).
pub fn starts_appendix(lines: &[String]) -> bool {
    let head: Vec<String> = lines
        .iter()
        .filter(|l| !l.is_empty())
        .take(4)
        .map(|l| {
            l.split_whitespace()
                .map(|t| clean(t).to_uppercase())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    (0..head.len()).any(|i| {
        let joined = match head.get(i + 1) {
            Some(next) => format!("{} {next}", head[i]),
            None => head[i].clone(),
        };
        // The line starts with it, after a page number at most.
        joined.find("APPENDIX TO THE CONG").is_some_and(|at| {
            head[i].len() > at
                && joined[..at]
                    .split_whitespace()
                    .all(|t| t.chars().all(|c| c.is_ascii_digit()))
        })
    })
}

/// The printed page number: the IIIF canvas label when it is one (`493`,
/// `IV`), else a number alone at either end of the page's first line, when
/// that line is a number alone or a running head (`28 THE CONGRESSIONAL
/// GLOBE December 15`). `None` when neither says.
pub fn printed_page(label: Option<&str>, lines: &[String]) -> Option<String> {
    if let Some(l) = label.map(str::trim) {
        let roman = !l.is_empty() && l.chars().all(|c| "IVXLCivxlc".contains(c)) && l.len() <= 8;
        if (!l.is_empty() && l.len() <= 5 && l.chars().all(|c| c.is_ascii_digit())) || roman {
            return Some(l.to_owned());
        }
    }
    let first = lines.iter().find(|l| !l.is_empty())?;
    let tokens: Vec<&str> = first.split_whitespace().map(clean).collect();
    let number = |t: &str| !t.is_empty() && t.len() <= 4 && t.chars().all(|c| c.is_ascii_digit());
    match tokens.as_slice() {
        [n] if number(n) => Some((*n).to_owned()),
        [n, next, ..] if number(n) && !number(next) && next.len() > 1 => first
            .to_uppercase()
            .contains("GLOBE")
            .then(|| (*n).to_owned()),
        [.., prev, n] if number(n) && !number(prev) && prev.len() > 1 => first
            .to_uppercase()
            .contains("GLOBE")
            .then(|| (*n).to_owned()),
        _ => None,
    }
}

/// A member a page names: `Mr. CLAY of Alabama`, `Mr. BIRDSALL` (#229
/// design §6). Matching to the congress-legislators ids comes later; until
/// then `bioguide` and `icpsr` are empty and `how` says only whether the
/// text named the state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Speaker {
    /// As printed: `CLAY of Alabama`.
    pub as_printed: String,
    pub surname: String,
    /// USPS code, when the text names the state.
    pub state: Option<String>,
    pub bioguide: Option<String>,
    pub icpsr: Option<u32>,
    /// `surname_state` (the text names the state) or `unresolved`.
    pub how: String,
}

/// Words after `Mr.` that are offices, not members.
const OFFICES: [&str; 5] = ["PRESIDENT", "SPEAKER", "CHAIRMAN", "CLERK", "SECRETARY"];

/// A surname as the Globe prints a member's: in capitals, two letters or
/// more, with a mixed-case prefix allowed (`McKAY`, `DeGRAFF`).
fn surname(token: &str) -> Option<&str> {
    let t = token.trim_matches(|c: char| !c.is_alphabetic());
    let rest = ["Mc", "Mac", "De", "Van", "Du", "La", "Le"]
        .iter()
        .find_map(|p| t.strip_prefix(p))
        .unwrap_or(t);
    let letters = rest.chars().filter(|c| c.is_alphabetic()).count();
    (letters >= 2
        && rest
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '\'' || c == '-')
        && !OFFICES.contains(&t))
    .then_some(t)
}

/// The members a page's text names, each once, in order.
pub fn speakers(text: &str) -> Vec<Speaker> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<Speaker> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if !matches!(*t, "Mr" | "Mr." | "MR" | "MR.") {
            continue;
        }
        let Some(name) = tokens.get(i + 1).and_then(|n| surname(n)) else {
            continue;
        };
        let mut state = None;
        let mut printed = name.to_owned();
        if tokens.get(i + 2).map(|t| clean(t)) == Some("of") {
            let one = tokens.get(i + 3).map(|t| clean(t)).unwrap_or_default();
            let two = tokens
                .get(i + 4)
                .map(|t| format!("{one} {}", clean(t)))
                .unwrap_or_default();
            if let Some(s) = usnm_core::names::state_by_name(&two).filter(|_| !two.is_empty()) {
                state = Some(s.code.to_owned());
                printed = format!("{name} of {two}");
            } else if let Some(s) = usnm_core::names::state_by_name(one).filter(|_| !one.is_empty())
            {
                state = Some(s.code.to_owned());
                printed = format!("{name} of {one}");
            }
        }
        let s = Speaker {
            as_printed: printed,
            surname: name.to_owned(),
            how: if state.is_some() {
                "surname_state".into()
            } else {
                "unresolved".into()
            },
            state,
            bioguide: None,
            icpsr: None,
        };
        if !out
            .iter()
            .any(|o| o.surname == s.surname && o.state == s.state)
        {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(y, m, day)
    }

    #[test]
    fn day_headings_damaged_or_not() {
        let h = heading("TftuasDAY March 29 1838").unwrap();
        assert_eq!(h.date, d(1838, 3, 29));
        assert_eq!(h.chamber, None);
        assert_eq!(
            heading("THURSDAY, March 29, 1838.").unwrap().date,
            d(1838, 3, 29)
        );
        assert_eq!(
            heading("Tuesday, December 15, 1863").unwrap().date,
            d(1863, 12, 15)
        );
        // A weekday spelled out in full must match the date.
        assert_eq!(heading("FRIDAY March 29 1838"), None);
        // Words that end in "day" but aren't weekdays.
        for w in [
            "YESTERDAY",
            "HOLIDAY",
            "MIDDAY",
            "WEEKDAY",
            "BIRTHDAY",
            "SOMEDAY",
        ] {
            assert_eq!(heading(&format!("{w} March 29 1838")), None, "{w}");
        }
        // Damaged weekdays still are.
        for w in ["Thursdav", "THUBSDAY", "TftuasDAY"] {
            assert_eq!(
                heading(&format!("{w} March 29 1838")).unwrap().date,
                d(1838, 3, 29),
                "{w}"
            );
        }
        // Prose isn't a heading.
        assert_eq!(heading("the committee met on Thursday March 29 1838"), None);
        assert_eq!(heading("Washington March 29 1838"), None);
        assert_eq!(heading("March 29 1838"), None);
        // Out of the debates' years.
        assert_eq!(heading("THURSDAY March 29 1938"), None);
        // A masthead's date is the issue's, not its debates'.
        assert_eq!(
            heading("25th Cokg 2nd Sess MONDAY APRIL 9 1838 Volume 6 No 18"),
            None
        );
    }

    #[test]
    fn chamber_headings_in_capitals() {
        assert_eq!(
            heading("HOUSE OP REPRESENTATIVES").unwrap().chamber,
            Some(Chamber::House)
        );
        assert_eq!(heading("IN SENATE").unwrap().chamber, Some(Chamber::Senate));
        assert_eq!(heading("SENATE.").unwrap().chamber, Some(Chamber::Senate));
        assert_eq!(heading("Senate"), None);
        assert_eq!(heading("the House of Representatives"), None);
        let h = heading("In Senate, March 3, 1838").unwrap();
        assert_eq!((h.date, h.chamber), (d(1838, 3, 3), Some(Chamber::Senate)));
        let h = heading("IN THE HOUSE OF REPRESENTATIVES Saturday March 3 1838").unwrap();
        assert_eq!((h.date, h.chamber), (d(1838, 3, 3), Some(Chamber::House)));
    }

    #[test]
    fn the_appendix_by_its_title_or_running_head() {
        let lines = |s: &[&str]| s.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>();
        assert!(starts_appendix(&lines(&[
            "APPENDIX",
            "TO THE CONG1BSSIOÍÍAL GLOBE",
            "25rH Co g 2nd Sess"
        ])));
        assert!(starts_appendix(&lines(&[
            "260",
            "5irf Cong",
            "",
            "APPENDIX TO THE CONGRESSIONAL ÓLdÉÉ Mar nh"
        ])));
        assert!(!starts_appendix(&lines(&[
            "INDEX TO THE APPENDIX",
            "TO THE CONGRESSIONAL GLOBE"
        ])));
        assert!(!starts_appendix(&lines(&[
            "28 THE CONGRESSIONAL GLOBE December 15",
            "the appendix to the Congressional Globe"
        ])));
    }

    #[test]
    fn printed_pages_from_labels_or_running_heads() {
        let l = |s: &str| vec![s.to_owned()];
        assert_eq!(printed_page(Some("493"), &[]), Some("493".into()));
        assert_eq!(printed_page(Some("IV"), &[]), Some("IV".into()));
        assert_eq!(
            printed_page(Some("Image 300"), &l("260")),
            Some("260".into())
        );
        assert_eq!(
            printed_page(None, &l("28 THE CONGRESSIONAL GLOBE December 15")),
            Some("28".into())
        );
        assert_eq!(printed_page(None, &l("48 3 r CONGRESSIONAL GLOBE")), None);
        assert_eq!(printed_page(None, &l("mtmimmM mm 485")), None);
        assert_eq!(
            printed_page(Some("No Page # [Sequence: 1]"), &l("aya ____")),
            None
        );
    }

    #[test]
    fn members_with_and_without_their_state() {
        let s = speakers(
            "Mr CLAY of Alabama gave notice. Mr DeGRAFF presented a memorial. Mr B also \
             asked leave. Mr. PRESIDENT, Mr CLAY of Alabama again; Mr SMITH of New York, \
             Mr McKAY replied",
        );
        let printed: Vec<&str> = s.iter().map(|s| s.as_printed.as_str()).collect();
        assert_eq!(
            printed,
            ["CLAY of Alabama", "DeGRAFF", "SMITH of New York", "McKAY"]
        );
        assert_eq!(s[0].state.as_deref(), Some("AL"));
        assert_eq!(s[0].how, "surname_state");
        assert_eq!(s[1].how, "unresolved");
        assert_eq!(s[2].state.as_deref(), Some("NY"));
    }

    #[test]
    fn an_annotation_list_needs_its_resources() {
        assert!(page_lines(b"{}").is_err());
        assert!(page_lines(br#"{"@type": "sc:AnnotationList"}"#).is_err());
        assert_eq!(
            page_lines(br#"{"resources": []}"#).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn lines_follow_the_word_boxes() {
        let json = br##"{"resources": [
            {"resource": {"chars": "The"}, "on": "c#xywh=50,100,30,20"},
            {"resource": {"chars": "bill"}, "on": "c#xywh=90,101,30,20"},
            {"resource": {"chars": "passed"}, "on": "c#xywh=50,125,60,20"},
            {"resource": {"chars": "  "}, "on": "c#xywh=130,125,60,20"},
            {"resource": {"chars": "SENATE"}, "on": "c#xywh=600,100,60,20"}
        ]}"##;
        assert_eq!(
            page_lines(json).unwrap(),
            ["The bill", "passed", "", "SENATE"]
        );
    }
}
