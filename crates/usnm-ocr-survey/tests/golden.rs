//! The Rust scorer against gold.py: the same synthetic inputs must give the same files, byte for
//! byte. scripts/ocr-survey/golden_score.py writes tests/golden/ (inputs and gold.py's outputs).
use std::path::{Path, PathBuf};
use usnm_ocr_survey::score;

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

#[test]
fn rust_scorer_matches_gold_py() {
    let root = golden();
    let rows = score::read_jsonl(&root.join("input/crops.jsonl")).unwrap();
    let mut cases = 0;
    for entry in std::fs::read_dir(root.join("expected")).unwrap() {
        let exp = entry.unwrap().path();
        let args: Vec<String> =
            serde_json::from_slice(&std::fs::read(exp.join("args.json")).unwrap()).unwrap();
        let (mut hyp, mut hints, mut review) = (Vec::new(), false, true);
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--hyp" => {
                    let (name, path) = it.next().unwrap().split_once('=').unwrap();
                    hyp.push(format!("{name}={}", root.join(path).display()));
                }
                "--hints" => hints = true,
                "--allow-unreviewed" => review = false,
                other => panic!("unexpected flag {other}"),
            }
        }
        let hyps = score::load_hyps(&rows, &hyp, hints).unwrap();
        let s = score::score(&rows, &hyps, review).unwrap();
        let name = exp.file_name().unwrap().to_string_lossy().to_string();
        let read = |f: &str| std::fs::read_to_string(exp.join(f)).unwrap();
        assert_eq!(
            score::by_crop_csv(&s.by_crop),
            read("cer-by-crop.csv"),
            "{name}: cer-by-crop.csv"
        );
        assert_eq!(
            score::summary_csv(&s.summary),
            read("cer-summary.csv"),
            "{name}: cer-summary.csv"
        );
        assert_eq!(score::table(&s.summary), read("table.txt"), "{name}: table");
        cases += 1;
    }
    assert_eq!(cases, 3);
}
