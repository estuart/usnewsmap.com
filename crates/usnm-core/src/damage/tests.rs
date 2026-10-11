//! `ja-ocr/test_quality.py`'s detection and damage tests, ported. The
//! parity fixture (`tests/damage_parity.rs`) checks the numbers against
//! Python's on real pages.

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
    ] {
        let got = score(text, languages);
        assert_eq!(
            (got.decision, got.lang.as_str()),
            (Decision::Detected, want),
            "{languages:?}"
        );
        assert!(got.function_share.unwrap() > 0.3, "{want}");
        assert_eq!(got.damage_rate, Some(0.0), "{want}");
    }
}

#[test]
fn a_language_the_title_does_not_list_is_not_found() {
    assert_eq!(score(GERMAN_TEXT, &["eng"]).decision, Decision::Und);
}

#[test]
fn languages_without_a_word_list_are_und() {
    let got = score(HAWAIIAN_TEXT, &["cho", "eng"]);
    assert_eq!(
        (
            got.decision,
            got.lang.as_str(),
            got.function_share,
            got.damage_rate
        ),
        (Decision::Und, "und", None, None)
    );
}

#[test]
fn hawaiian_has_our_own_word_list() {
    let got = score(HAWAIIAN_TEXT, &["haw", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.damage_rate),
        (Decision::Detected, "haw", Some(0.0))
    );
    assert!(got.function_share.unwrap() > 0.3);
    let garbled = HAWAIIAN_TEXT
        .replace(" ka ", " kn ")
        .replace(" na ", " nn ")
        .replace(" ua ", " un ");
    assert!(score(&garbled, &["haw", "eng"]).damage_rate.unwrap() > 0.1);
}

#[test]
fn too_short_or_garbled_is_und() {
    let got = score(
        "The council met on Tuesday and voted for the new school.",
        &["eng"],
    );
    assert_eq!((got.decision, got.lang.as_str()), (Decision::Und, "und"));
    let garbled = rep("tlrc 8ii nrw Iii fkm Jtk lttk abr tbW4i kikf Jíl", 10);
    assert_eq!(score(&garbled, &["eng", "cze"]).decision, Decision::Und);
}

#[test]
fn bilingual_page_is_mixed() {
    let text = format!("{} {}", rep(GERMAN_TEXT, 3), rep(ENGLISH_TEXT, 3));
    let got = score(&text, &["ger", "eng"]);
    assert_eq!(got.decision, Decision::Mixed);
    let mut pair = [got.lang.clone(), got.mixed_with.clone().unwrap()];
    pair.sort();
    assert_eq!(pair, ["eng", "ger"]);
    assert!(got.function_share.unwrap() > 0.3);
    let got = score(
        &format!("{} {ENGLISH_TEXT}", rep(GERMAN_TEXT, 6)),
        &["ger", "eng"],
    );
    assert_eq!(
        (got.decision, got.lang.as_str()),
        (Decision::Detected, "ger")
    );
}

#[test]
fn hebrew_script() {
    let yiddish = score(YIDDISH_TEXT, &["eng", "yid"]);
    assert_eq!(
        (yiddish.decision, yiddish.lang.as_str(), yiddish.script),
        (Decision::Detected, "yid", "hebrew")
    );
    assert_eq!(yiddish.damage_rate, Some(0.0));
    assert!(yiddish.function_share.unwrap() > 0.3);
    assert_eq!(score(YIDDISH_TEXT, &["eng"]).decision, Decision::Und);
    assert_eq!(score(YIDDISH_TEXT, &["heb", "yid"]).lang, "yid");
    let hebrew = score(YIDDISH_TEXT, &["heb"]);
    assert_eq!((hebrew.lang.as_str(), hebrew.script), ("heb", "hebrew"));
    let both = score(&format!("{YIDDISH_TEXT} {ENGLISH_TEXT}"), &["yid", "eng"]);
    assert_eq!(both.decision, Decision::Mixed);
}

#[test]
fn windows_are_fixed_50_word_slices() {
    let owned: Vec<String> = (0..240).map(|i| format!("w{i}")).collect();
    let ws: Vec<&str> = owned.iter().map(String::as_str).collect();
    assert_eq!(
        windows_of(&ws).iter().map(|w| w.len()).collect::<Vec<_>>(),
        [50, 50, 50, 50, 40]
    );
    assert_eq!(
        windows_of(&ws[..210])
            .iter()
            .map(|w| w.len())
            .collect::<Vec<_>>(),
        [50, 50, 50, 50]
    );
    assert!(windows_of(&ws[..199]).is_empty());
}

#[test]
fn english_then_german_is_mixed() {
    let text = format!("{} {}", words(ENGLISH_TEXT, 140), words(GERMAN_TEXT, 100));
    assert_eq!(text_words(&text).words.len(), 240);
    let got = score(&text, &["ger", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.mixed_with.as_deref()),
        (Decision::Mixed, "eng", Some("ger"))
    );
}

#[test]
fn undecided_windows_are_not_scored() {
    let garbled = rep("tbe aud nnd tlie thc", 10);
    let text = format!(
        "{} {} {garbled}",
        words(ENGLISH_TEXT, 150),
        words(GERMAN_TEXT, 100)
    );
    let got = score(&text, &["ger", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.mixed_with.as_deref()),
        (Decision::Mixed, "eng", Some("ger"))
    );
    assert_eq!(got.damage_rate, Some(0.0));
    let clean = score(
        &format!("{} {}", words(ENGLISH_TEXT, 150), words(GERMAN_TEXT, 100)),
        &["ger", "eng"],
    );
    assert_eq!(got.function_share, clean.function_share);
}

#[test]
fn a_short_second_script_part_makes_a_page_mixed() {
    let text = format!(
        "{} the of and to in is for that it was",
        words(YIDDISH_TEXT, 40)
    );
    let got = score(&text, &["yid", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.mixed_with.as_deref()),
        (Decision::Mixed, "yid", Some("eng"))
    );
    let d = |t: String| score(&t, &["yid", "eng"]).decision;
    assert_eq!(
        d(format!("{} the of and to in", words(YIDDISH_TEXT, 45))),
        Decision::Detected
    );
    assert_eq!(
        d(text.replace("that it was", "the the the")),
        Decision::Mixed
    );
    assert_eq!(
        d(format!(
            "{} the the the the of of of and and and",
            words(YIDDISH_TEXT, 40)
        )),
        Decision::Detected
    );
    assert_eq!(
        score(&words(YIDDISH_TEXT, 15), &["yid"]).decision,
        Decision::Und
    );
}

#[test]
fn cyrillic_serbian_is_scored_in_latin_letters() {
    let got = score(SERBIAN_TEXT, &["srp", "eng"]);
    assert_eq!(
        (got.decision, got.lang.as_str(), got.script, got.damage_rate),
        (Decision::Detected, "srp", "cyrillic", Some(0.0))
    );
    assert!(got.function_share.unwrap() > 0.2);
    assert_eq!(serbian_latin("љубав њега џеп ђак"), "ljubav njega džep đak");
}

#[test]
fn serbian_competes_with_another_cyrillic_language() {
    let got = score(SERBIAN_TEXT, &["rus", "srp"]);
    assert_eq!((got.lang.as_str(), got.damage_rate), ("srp", Some(0.0)));
    assert_eq!(score(RUSSIAN_TEXT, &["srp", "rus"]).lang, "rus");
}

#[test]
fn rate() {
    let en = model("eng").unwrap();
    let d = damage(
        en,
        "the tbe the and aud of ot she an council".split(' '),
        &[],
    );
    assert_eq!((d.head, d.near, d.words), (4, 3, 10));
    assert!((d.damage_rate().unwrap() - 3.0 / 7.0).abs() < 1e-12);
    assert!((d.function_share().unwrap() - 0.6).abs() < 1e-12);
    let de = model("ger").unwrap();
    assert_eq!(
        damage(
            de,
            "die bie der dcr und unb ist ift sich fich".split(' '),
            &[]
        )
        .damage_rate(),
        Some(0.5)
    );
    assert_eq!(damage(en, ["council"], &[]).damage_rate(), None);
    // "tie" is one edit from "die", but an English word.
    assert_eq!(damage(de, ["tie"], &[]).near, 1);
    assert_eq!(damage(de, ["tie"], &[en]).near, 0);
}

#[test]
fn damaged_page() {
    let damaged = ENGLISH_TEXT.replace("the ", "tbe ").replace("and ", "aud ");
    let got = score(&damaged, &["eng"]);
    assert_eq!(got.lang, "eng");
    assert!(got.damage_rate.unwrap() > BADLY_DAMAGED);
}

#[test]
fn forced_language() {
    let got = score_forced(GERMAN_TEXT, "deu").unwrap();
    assert_eq!(got.damage_rate, Some(0.0));
    assert!(got.known_words > 0);
    let damaged = score_forced(&GERMAN_TEXT.replace("die ", "bie "), "deu").unwrap();
    assert!(damaged.damage_rate.unwrap() > 0.1);
    let srp = score_forced(SERBIAN_TEXT, "srp").unwrap();
    assert_eq!(srp.damage_rate, Some(0.0));
    assert!(
        score_forced(ENGLISH_TEXT, "ger").is_none(),
        "MARC codes aren't ISO 639-3"
    );
    assert!(score_forced(ENGLISH_TEXT, "zho").is_none(), "no word list");
}
