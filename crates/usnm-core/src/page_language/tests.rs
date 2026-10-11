//! `ja-ocr/test_quality.py`'s detection tests, ported (as the damage
//! metric's are), and the index's page languages. The parity test
//! (`tests/page_language_parity.rs`) checks the results against Python's on
//! real pages.

use super::*;

const ENGLISH_TEXT: &str = "The council of the city met on Tuesday evening at the court house, and after a long debate it was voted \
that the new school should be built on the lot which the board had bought from Mr. Smith last year. The \
mayor said that he would not sign the bill unless the cost of the work was paid from the general fund, and \
the members agreed with him.";
const GERMAN_TEXT: &str = "Der Stadtrath hat am Dienstag beschlossen, dass die neue Schule auf dem Grundstück gebaut werden soll, \
welches die Behörde im vorigen Jahre von Herrn Schmidt gekauft hat. Der Bürgermeister sagte, er werde das \
Gesetz nicht unterzeichnen, wenn die Kosten nicht aus der allgemeinen Kasse bezahlt würden, und die \
Mitglieder waren mit ihm einverstanden. Es ist noch nicht bekannt, wann die Arbeit beginnen wird.";
const FRENCH_TEXT: &str = "Le conseil de la ville s'est réuni mardi soir au palais de justice, et après un long débat il a été décidé \
que la nouvelle école sera construite sur le terrain que la commission avait acheté l'année dernière. Le \
maire a dit qu'il ne signerait pas le projet si le coût des travaux n'était pas payé par le fonds général, \
et les membres étaient d'accord avec lui. On ne sait pas encore quand les travaux vont commencer.";
const SPANISH_TEXT: &str = "El consejo de la ciudad se reunió el martes por la noche en la casa de justicia, y después de un largo \
debate se decidió que la nueva escuela será construida en el terreno que la junta había comprado el año \
pasado. El alcalde dijo que no firmaría la ley si el costo de la obra no fuera pagado por el fondo general, y \
los miembros estuvieron de acuerdo con él. Todavía no se sabe cuándo comenzará el trabajo.";
const POLISH_TEXT: &str = "Rada miejska zebrała się we wtorek wieczorem w sądzie i po długiej debacie postanowiono, że nowa szkoła \
będzie zbudowana na placu, który zarząd kupił w zeszłym roku. Burmistrz powiedział, że nie podpisze ustawy, \
jeżeli koszt pracy nie będzie pokryty z ogólnego funduszu, a członkowie zgodzili się z nim. Jeszcze nie \
wiadomo, kiedy praca się rozpocznie, ale wszyscy są tego zdania, że to jest dobra rzecz dla miasta i dla \
jego mieszkańców.";
const YIDDISH_TEXT: &str = "דער שטאט ראט האט זיך פֿארזאמלט דינסטיק אין אוונט און נאך א לאנגע וויכוח האט מען באשלאסן אז די נייע \
שול וועט געבויט ווערן אויף דעם פלאץ וואס די קאמיסיע האט געקויפט פאר א יאר. דער מעיאר האט געזאגט אז ער \
וועט נישט אונטערשרייבן דעם געזעץ אויב די קאסטן וועלן נישט באצאלט ווערן פון דער אלגעמיינער קאסע.";
const HAWAIIAN_TEXT: &str = "Ua hiki mai ka moku mai Kaleponi mai i ka la mua o keia mahina, a ua lawe mai oia i na leka he nui loa no \
na kanaka o keia aina. Ua olelo ia mai e hele ana ke Alii i Hawaii i keia pule aku, a e noho ana oia ma \
Hilo no kekahi manawa. Ua nui ka ua ma Honolulu i keia pule, a ua piha na kahawai a pau.";
const SERBIAN_TEXT: &str =
    "Општински одбор састао се у уторак увече у судници и после дуге расправе решено је да се \
нова школа сагради на плацу који је управа купила прошле године. Председник општине рекао је \
да неће потписати закон ако трошкови не буду плаћени из општег фонда.";
const RUSSIAN_TEXT: &str =
    "Городской совет собрался во вторник вечером в здании суда и после долгого спора было решено, \
что новая школа будет построена на участке, который управление купило в прошлом году. \
Председатель сказал, что он не подпишет закон, если расходы не будут оплачены из общего фонда.";

/// The first `n` word tokens of `text`, repeated as needed.
fn words(text: &str, n: usize) -> String {
    let one = text_words(text).words;
    one.iter()
        .cycle()
        .take(n)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

fn rep(text: &str, n: usize) -> String {
    vec![text; n].join(" ")
}

fn lang(text: &str, languages: &[&str]) -> (Decision, String) {
    let d = detect(text, languages);
    (d.decision, d.lang)
}

#[test]
fn each_language_among_the_titles() {
    for (text, languages, want) in [
        (ENGLISH_TEXT, &["eng", "ger"][..], "eng"),
        (GERMAN_TEXT, &["ger", "eng"], "ger"),
        (GERMAN_TEXT, &["eng", "ger"], "ger"),
        (FRENCH_TEXT, &["fre", "spa", "eng"], "fre"),
        (SPANISH_TEXT, &["spa", "fre"], "spa"),
        (POLISH_TEXT, &["eng", "pol"], "pol"),
        (ENGLISH_TEXT, &[], "eng"),
        (ENGLISH_TEXT, &["haw"], "eng"),
        (HAWAIIAN_TEXT, &["haw", "eng"], "haw"),
    ] {
        let got = detect(text, languages);
        assert_eq!(
            (got.decision, got.lang.as_str()),
            (Decision::Detected, want),
            "{languages:?}"
        );
        assert!(got.share.unwrap() > 0.3, "{want}");
    }
}

#[test]
fn shares_of_the_winner_and_the_runner_up() {
    let got = detect(GERMAN_TEXT, &["ger", "eng"]);
    assert_eq!(got.runner_up.as_deref(), Some("eng"));
    assert!(got.share.unwrap() > got.runner_up_share.unwrap());
    let alone = detect(GERMAN_TEXT, &["ger"]);
    assert_eq!(
        alone.runner_up.as_deref(),
        Some("eng"),
        "English always competes"
    );
}

#[test]
fn und_pages() {
    // A language the title doesn't list, one with no word list, too short, garbled.
    assert_eq!(detect(GERMAN_TEXT, &["eng"]).decision, Decision::Und);
    let got = detect(HAWAIIAN_TEXT, &["cho", "eng"]);
    assert_eq!((got.decision, got.lang.as_str()), (Decision::Und, "und"));
    let short = detect(
        "The council met on Tuesday and voted for the new school.",
        &["eng"],
    );
    assert_eq!(
        (short.decision, short.script, short.share),
        (Decision::Und, "latin", None)
    );
    let garbled = rep("tlrc 8ii nrw Iii fkm Jtk lttk abr tbW4i kikf Jíl", 10);
    assert_eq!(detect(&garbled, &["eng", "cze"]).decision, Decision::Und);
    assert_eq!(detect("", &["eng"]).decision, Decision::Und);
}

#[test]
fn bilingual_page_is_mixed() {
    let text = format!("{} {}", rep(GERMAN_TEXT, 3), rep(ENGLISH_TEXT, 3));
    let got = detect(&text, &["ger", "eng"]);
    assert_eq!(got.decision, Decision::Mixed);
    let mut pair = [got.lang.clone(), got.mixed_with.clone().unwrap()];
    pair.sort();
    assert_eq!(pair, ["eng", "ger"]);
    assert_eq!(got.runner_up, got.mixed_with);
    assert_eq!(
        lang(
            &format!("{} {ENGLISH_TEXT}", rep(GERMAN_TEXT, 6)),
            &["ger", "eng"]
        ),
        (Decision::Detected, "ger".to_owned())
    );
    let text = format!("{} {}", words(ENGLISH_TEXT, 140), words(GERMAN_TEXT, 100));
    let got = detect(&text, &["ger", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.mixed_with.as_deref()),
        (Decision::Mixed, "eng", Some("ger"))
    );
}

#[test]
fn windows_are_fixed_50_word_slices() {
    let owned: Vec<String> = (0..240).map(|i| format!("w{i}")).collect();
    let ws: Vec<&str> = owned.iter().map(String::as_str).collect();
    let lens = |w: &[&str]| windows_of(w).iter().map(|w| w.len()).collect::<Vec<_>>();
    assert_eq!(lens(&ws), [50, 50, 50, 50, 40]);
    assert_eq!(lens(&ws[..210]), [50, 50, 50, 50]);
    assert!(windows_of(&ws[..199]).is_empty());
}

#[test]
fn hebrew_script() {
    let yiddish = detect(YIDDISH_TEXT, &["eng", "yid"]);
    assert_eq!(
        (yiddish.decision, yiddish.lang.as_str(), yiddish.script),
        (Decision::Detected, "yid", "hebrew")
    );
    assert_eq!(detect(YIDDISH_TEXT, &["eng"]).decision, Decision::Und);
    assert_eq!(detect(YIDDISH_TEXT, &["heb", "yid"]).lang, "yid");
    let hebrew = detect(YIDDISH_TEXT, &["heb"]);
    assert_eq!((hebrew.lang.as_str(), hebrew.script), ("heb", "hebrew"));
    // Ladino has no list: the only Hebrew-script language without one wins.
    assert_eq!(detect(YIDDISH_TEXT, &["lad", "rus"]).lang, "lad");
    let both = detect(&format!("{YIDDISH_TEXT} {ENGLISH_TEXT}"), &["yid", "eng"]);
    assert_eq!(both.decision, Decision::Mixed);
}

#[test]
fn a_short_second_script_part_makes_a_page_mixed() {
    let text = format!(
        "{} the of and to in is for that it was",
        words(YIDDISH_TEXT, 40)
    );
    let got = detect(&text, &["yid", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.mixed_with.as_deref()),
        (Decision::Mixed, "yid", Some("eng"))
    );
    let d = |t: String| detect(&t, &["yid", "eng"]).decision;
    assert_eq!(
        d(format!("{} the of and to in", words(YIDDISH_TEXT, 45))),
        Decision::Detected
    );
    assert_eq!(
        d(format!(
            "{} the the the the of of of and and and",
            words(YIDDISH_TEXT, 40)
        )),
        Decision::Detected
    );
    assert_eq!(
        detect(&words(YIDDISH_TEXT, 15), &["yid"]).decision,
        Decision::Und
    );
}

#[test]
fn cyrillic() {
    let got = detect(SERBIAN_TEXT, &["srp", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.script),
        (Decision::Detected, "srp", "cyrillic")
    );
    assert!(got.share.unwrap() > 0.2);
    assert_eq!(serbian_latin("љубав њега џеп ђак"), "ljubav njega džep đak");
    assert_eq!(detect(SERBIAN_TEXT, &["rus", "srp"]).lang, "srp");
    assert_eq!(detect(RUSSIAN_TEXT, &["srp", "rus"]).lang, "rus");
    // Croatian shares Serbian's list, but isn't written in Cyrillic.
    assert_eq!(
        detect(SERBIAN_TEXT, &["hrv", "eng"]).decision,
        Decision::Und
    );
}

#[test]
fn a_script_with_no_list_takes_the_title_language() {
    let cherokee = rep("ᏣᎳᎩ ᎦᏬᏂᎯᏍᏗ ᏧᏬᏪᎳᏅᎯ ᎠᏂᏴᏫᏯ", 8);
    let got = detect(&cherokee, &["chr", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.script, got.share),
        (Decision::Detected, "chr", "cherokee", None)
    );
    assert_eq!(detect(&cherokee, &["eng"]).decision, Decision::Und);
}

fn det(decision: Decision, lang: &str, mixed_with: Option<&str>) -> Detection {
    Detection {
        lang: lang.to_owned(),
        decision,
        mixed_with: mixed_with.map(str::to_owned),
        script: "latin",
        share: None,
        runner_up: None,
        runner_up_share: None,
    }
}

#[test]
fn page_languages_follow_the_decisions() {
    let both = ["eng", "ger"];
    let got = |d: Option<&Detection>, title: &[&str]| page_languages(d, title);
    let found = |l: &str| det(Decision::Detected, l, None);
    let und = det(Decision::Und, UNKNOWN, None);
    assert_eq!(got(Some(&found("ger")), &both), ["ger"]);
    // A single-language title's page found in English.
    assert_eq!(got(Some(&found("eng")), &["ger"]), ["eng"]);
    assert_eq!(
        got(Some(&det(Decision::Mixed, "ger", Some("eng"))), &["ger"]),
        ["eng", "ger"]
    );
    // Undetermined, or no text: the title's languages, sorted.
    assert_eq!(got(Some(&und), &["ger", "eng"]), both);
    assert_eq!(got(None, &["ger", "eng"]), both);
    assert!(got(None, &[]).is_empty());
    assert!(got(Some(&und), &[]).is_empty());
    // Languages with no word list stay, whatever the result.
    assert_eq!(got(Some(&found("eng")), &["dak", "eng"]), ["dak", "eng"]);
    assert_eq!(got(Some(&found("eng")), &["jpn", "eng"]), ["eng", "jpn"]);
    assert_eq!(got(Some(&found("chr")), &["chr", "eng"]), ["chr"]);
    assert_eq!(got(None, &["eng", "eng"]), ["eng"]);
}

#[test]
fn page_languages_of_real_text() {
    let langs = |text: &str, title: &[&str]| page_languages(Some(&detect(text, title)), title);
    assert_eq!(langs(GERMAN_TEXT, &["eng", "ger"]), ["ger"]);
    let mixed = format!("{} {}", rep(GERMAN_TEXT, 3), rep(ENGLISH_TEXT, 3));
    assert_eq!(langs(&mixed, &["ger"]), ["eng", "ger"]);
    assert_eq!(langs("too short", &["ger", "eng"]), ["eng", "ger"]);
}

#[test]
fn set_keys() {
    assert_eq!(set_key(&["eng"]), "eng");
    assert_eq!(set_key(&["eng", "ger"]), "eng_ger");
    assert_eq!(set_key::<&str>(&[]), NO_LANGUAGE);
    assert_eq!(set_languages("eng_ger").collect::<Vec<_>>(), ["eng", "ger"]);
    assert_eq!(set_languages(NO_LANGUAGE).count(), 0);
    for (code, ok) in [
        ("eng", true),
        ("en", false),
        ("ENG", false),
        ("e_g", false),
        ("none", false),
    ] {
        assert_eq!(is_code(code), ok, "{code}");
    }
}
