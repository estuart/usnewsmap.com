//! The three-cornered hat: each reader's own error rate from the pairwise
//! disagreements, without a transcription.
//!
//! For readers with independent errors, the disagreement between two of
//! them is about the sum of their error rates, `d_ij = e_i + e_j`. Three
//! readers give three equations in three unknowns, solved exactly:
//! `e_A = (d_AB + d_AC - d_BC) / 2`. Four or more give more pairs than
//! unknowns, fitted by least squares with a residual that says how far the
//! model is from the data. Correlated failures (every reader fails on the
//! same smudge) make the disagreements smaller than the sums, so the
//! estimates are biased low; calibration on public ground truth measures
//! that bias.
//!
//! The normal equations are solved by Gaussian elimination with partial
//! pivoting, in a fixed order, so the result is a function of the inputs.
use serde::{Deserialize, Serialize};

/// What the hat found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hat {
    /// Readers in the system.
    pub n_readers: usize,
    /// Pairs the fit used.
    pub pairs: usize,
    /// The solved rates, clipped at 0, one per reader; empty when the
    /// system is underdetermined.
    pub estimates: Vec<f64>,
    /// The solved rates before clipping.
    pub raw: Vec<f64>,
    /// Root mean square of `d_ij - e_i - e_j` over the pairs; 0 for three
    /// readers with all three pairs.
    pub residual: f64,
    /// False when fewer than three readers, or too few pairs, were given.
    pub determined: bool,
}

/// Solves `d_ij = e_i + e_j` for `n` readers over `pairs` of
/// `(i, j, d_ij)` with `i != j`, each pair at most once.
pub fn solve(n: usize, pairs: &[(usize, usize, f64)]) -> Hat {
    let undetermined = Hat {
        n_readers: n,
        pairs: pairs.len(),
        estimates: vec![],
        raw: vec![],
        residual: 0.0,
        determined: false,
    };
    if n < 3 || pairs.len() < n {
        return undetermined;
    }
    // Normal equations: (X^T X) e = X^T d, with X one row per pair holding
    // 1 in columns i and j.
    let mut ata = vec![vec![0.0f64; n]; n];
    let mut atd = vec![0.0f64; n];
    for &(i, j, d) in pairs {
        if i == j || i >= n || j >= n {
            return undetermined;
        }
        for (r, c) in [(i, i), (j, j), (i, j), (j, i)] {
            ata[r][c] += 1.0;
        }
        atd[i] += d;
        atd[j] += d;
    }
    let Some(raw) = gauss(ata, atd) else {
        return undetermined;
    };
    let estimates: Vec<f64> = raw.iter().map(|e| e.max(0.0)).collect();
    let ss: f64 = pairs
        .iter()
        .map(|&(i, j, d)| {
            let r = d - raw[i] - raw[j];
            r * r
        })
        .sum();
    Hat {
        n_readers: n,
        pairs: pairs.len(),
        estimates,
        raw,
        residual: (ss / pairs.len() as f64).sqrt(),
        determined: true,
    }
}

/// Gaussian elimination with partial pivoting; none for a singular system.
fn gauss(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let mut pivot = col;
        for r in col + 1..n {
            if a[r][col].abs() > a[pivot][col].abs() {
                pivot = r;
            }
        }
        if a[pivot][col].abs() < 1e-9 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        let pivot_row = a[col].clone();
        for r in col + 1..n {
            let f = a[r][col] / pivot_row[col];
            if f != 0.0 {
                for (x, p) in a[r].iter_mut().zip(&pivot_row).skip(col) {
                    *x -= f * p;
                }
                b[r] -= f * b[col];
            }
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let mut s = b[r];
        for c in r + 1..n {
            s -= a[r][c] * x[c];
        }
        x[r] = s / a[r][r];
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn unit(&mut self) -> f64 {
            (self.next() % 1_000_000) as f64 / 1_000_000.0
        }
    }

    /// A copy of `truth` with each character replaced, dropped or doubled
    /// independently at rate `p`.
    fn corrupt(truth: &str, p: f64, r: &mut Lcg) -> String {
        let mut out = String::new();
        for c in truth.chars() {
            if c != ' ' && r.unit() < p {
                match r.next() % 3 {
                    0 => out.push(if c == 'z' { 'a' } else { 'z' }),
                    1 => {}
                    _ => {
                        out.push(c);
                        out.push(c);
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn truth(r: &mut Lcg) -> String {
        let mut words = Vec::new();
        for _ in 0..1500 {
            let len = 2 + r.next() % 8;
            words.push(
                (0..len)
                    .map(|_| (b'a' + (r.next() % 26) as u8) as char)
                    .collect::<String>(),
            );
        }
        words.join(" ")
    }

    #[test]
    fn three_readers_exact() {
        // e = (0.1, 0.2, 0.3): d_AB 0.3, d_AC 0.4, d_BC 0.5.
        let h = solve(3, &[(0, 1, 0.3), (0, 2, 0.4), (1, 2, 0.5)]);
        assert!(h.determined);
        for (got, want) in h.estimates.iter().zip([0.1, 0.2, 0.3]) {
            assert!((got - want).abs() < 1e-12, "{:?}", h.estimates);
        }
        assert!(h.residual < 1e-12);
    }

    #[test]
    fn recovers_known_rates_on_synthetic_text() {
        let mut r = Lcg(7);
        let t = truth(&mut r);
        let rates = [0.02, 0.06, 0.12];
        let texts: Vec<String> = rates.iter().map(|p| corrupt(&t, *p, &mut r)).collect();
        let true_cer: Vec<f64> = texts.iter().map(|x| align::pair(x, &t).cer_a).collect();
        let mut pairs = Vec::new();
        for i in 0..3 {
            for j in i + 1..3 {
                pairs.push((i, j, align::pair(&texts[i], &texts[j]).cer_sym));
            }
        }
        let h = solve(3, &pairs);
        assert!(h.determined);
        for (i, (est, truth)) in h.estimates.iter().zip(&true_cer).enumerate() {
            assert!(
                (est - truth).abs() < 0.02 && (est - truth).abs() < 0.35 * truth,
                "reader {i}: estimated {est:.4}, true {truth:.4}"
            );
        }
    }

    #[test]
    fn four_readers_least_squares() {
        // Exact data: e = (0.05, 0.1, 0.2, 0.3) gives a zero residual and
        // the rates back.
        let e = [0.05, 0.1, 0.2, 0.3];
        let mut pairs = Vec::new();
        for i in 0..4 {
            for j in i + 1..4 {
                pairs.push((i, j, e[i] + e[j]));
            }
        }
        let h = solve(4, &pairs);
        assert!(h.determined);
        for (got, want) in h.estimates.iter().zip(e) {
            assert!((got - want).abs() < 1e-10);
        }
        assert!(h.residual < 1e-10);
        // Noisy data: a residual, estimates still near.
        let noisy: Vec<(usize, usize, f64)> = pairs
            .iter()
            .enumerate()
            .map(|(k, &(i, j, d))| (i, j, d + if k % 2 == 0 { 0.01 } else { -0.01 }))
            .collect();
        let h = solve(4, &noisy);
        assert!(h.residual > 0.0 && h.residual < 0.02);
        for (got, want) in h.estimates.iter().zip(e) {
            assert!((got - want).abs() < 0.02);
        }
    }

    #[test]
    fn degenerate_cases() {
        assert!(!solve(2, &[(0, 1, 0.3)]).determined);
        assert!(!solve(3, &[(0, 1, 0.3), (0, 2, 0.4)]).determined);
        assert!(!solve(3, &[(0, 1, 0.3), (0, 2, 0.4), (0, 0, 0.1)]).determined);
        // A reader far better than the others can solve below zero: clipped,
        // with the raw value kept.
        let h = solve(3, &[(0, 1, 0.1), (0, 2, 0.1), (1, 2, 0.5)]);
        assert_eq!(h.estimates[0], 0.0);
        assert!(h.raw[0] < 0.0);
        // Pairs that only connect two of three readers are singular.
        assert!(!solve(3, &[(0, 1, 0.3), (1, 0, 0.3), (0, 1, 0.3)]).determined);
    }
}
