//! A rendered line of capitals as a binary PGM, for tests that run the real
//! Tesseract without a checked-in image.

/// 5×7 glyphs for the letters the tests use.
fn glyph(c: char) -> [&'static str; 7] {
    match c {
        'D' => [
            "####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####.",
        ],
        'E' => [
            "#####", "#....", "#....", "####.", "#....", "#....", "#####",
        ],
        'H' => [
            "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
        ],
        'L' => [
            "#....", "#....", "#....", "#....", "#....", "#....", "#####",
        ],
        'O' => [
            ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
        ],
        'R' => [
            "####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#",
        ],
        'W' => [
            "#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#",
        ],
        _ => [
            ".....", ".....", ".....", ".....", ".....", ".....", ".....",
        ],
    }
}

/// `text` in black on white, each font pixel `SCALE` pixels square.
pub fn text_image(text: &str) -> Vec<u8> {
    const SCALE: usize = 6;
    const MARGIN: usize = 40;
    let cols = text.chars().count() * 6;
    let (w, h) = (cols * SCALE + 2 * MARGIN, 7 * SCALE + 2 * MARGIN);
    let mut px = vec![255u8; w * h];
    for (i, c) in text.chars().enumerate() {
        for (row, line) in glyph(c).iter().enumerate() {
            for (col, b) in line.bytes().enumerate() {
                if b != b'#' {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let x = MARGIN + (i * 6 + col) * SCALE + dx;
                        let y = MARGIN + row * SCALE + dy;
                        px[y * w + x] = 0;
                    }
                }
            }
        }
    }
    let mut out = format!("P5\n{w} {h}\n255\n").into_bytes();
    out.extend_from_slice(&px);
    out
}
