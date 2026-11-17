//! Repairing a disc with CUETools database parity (step 3 of
//! docs/ctdb-repair.md). Each of the 11760 columns is a Reed-Solomon
//! codeword over GF(2^16) whose syndromes the database holds; ours minus
//! theirs is the syndrome of our errors in that column, which an
//! errors-and-erasures decoder turns into positions and values.
//!
//! Beyond CUETools: we often know WHERE a damaged rip is doubtful (the
//! reads disagree there). Such positions are passed as erasures, which cost
//! half as much as unknown errors: per column 2·errors + erasures ≤ npar.
//!
//! Positions: data row r (1..=R, R = stridecount) is position p = R − r,
//! i.e. the word's weight in syndrome i is α^(i·p) (see
//! `ctdb::column_syndromes`).

use crate::ctdb::{GF_MAX, Gf16, Layout, STRIDE, gf};

/// Polynomials are coefficient vectors, lowest degree first.
fn poly_eval(g: &Gf16, p: &[u16], x: u16) -> u16 {
    // Horner from the top.
    let mut acc = 0u16;
    for &c in p.iter().rev() {
        acc = g.mul(acc, x) ^ c;
    }
    acc
}

fn poly_mul(g: &Gf16, a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = vec![0u16; a.len() + b.len() - 1];
    for (i, &x) in a.iter().enumerate() {
        if x == 0 {
            continue;
        }
        for (j, &y) in b.iter().enumerate() {
            out[i + j] ^= g.mul(x, y);
        }
    }
    out
}

/// α^p
fn alpha(g: &Gf16, p: usize) -> u16 {
    g.exp[p % GF_MAX]
}

/// Decodes one column. `syn` = our syndromes XOR theirs (npar of them),
/// `erasures` = positions known to be doubtful, `len` = positions in the
/// codeword (R). Returns the corrections (position, value to XOR in), or
/// None if the errors don't fit (nothing is changed then).
pub fn decode_column(syn: &[u16], erasures: &[usize], len: usize) -> Option<Vec<(usize, u16)>> {
    decode_column_spare(syn, erasures, len, 0)
}

/// [`decode_column`], but only accepting a decoding that leaves `spare`
/// syndromes unused (2·errors + erasures + spare ≤ npar): with none spare,
/// npar erasures "explain" any syndrome, right or not.
pub fn decode_column_spare(syn: &[u16], erasures: &[usize], len: usize, spare: usize) -> Option<Vec<(usize, u16)>> {
    let g = gf();
    let n = syn.len();
    if syn.iter().all(|&s| s == 0) {
        return Some(Vec::new());
    }
    if erasures.len() + spare > n {
        return None;
    }
    // Erasure locator Γ(x) = Π (1 − X_k x), X_k = α^p.
    let mut lambda: Vec<u16> = vec![1];
    for &p in erasures {
        lambda = poly_mul(g, &lambda, &[1, alpha(g, p)]);
    }
    let gamma = lambda.clone();
    // Berlekamp-Massey seeded with the erasure locator (Blahut).
    let rho = erasures.len();
    let mut b = lambda.clone();
    let mut l = rho;
    let mut m = 1usize;
    let mut bb: u16 = 1;
    for r in rho..n {
        let mut delta = 0u16;
        for (i, &c) in lambda.iter().enumerate() {
            if i <= r {
                delta ^= g.mul(c, syn[r - i]);
            }
        }
        if delta == 0 {
            m += 1;
            continue;
        }
        let coef = g.div(delta, bb);
        // λ(x) − (Δ/b)·x^m·B(x)
        let mut t = lambda.clone();
        if t.len() < b.len() + m {
            t.resize(b.len() + m, 0);
        }
        for (i, &c) in b.iter().enumerate() {
            t[i + m] ^= g.mul(coef, c);
        }
        if 2 * l <= r + rho {
            l = r + 1 + rho - l;
            b = lambda;
            bb = delta;
            m = 1;
        } else {
            m += 1;
        }
        lambda = t;
    }
    while lambda.len() > 1 && *lambda.last().unwrap() == 0 {
        lambda.pop();
    }
    let deg = lambda.len() - 1;
    // Errata beyond what the code can fix: 2·errors + erasures > n.
    if deg > n || 2 * (deg - rho.min(deg)) + rho + spare > n {
        return None;
    }
    // Chien search: positions whose X⁻¹ is a root of λ. When λ is the
    // erasure locator itself (no unknown errors), its roots are the erasures.
    let mut found = Vec::new();
    if deg == rho && lambda == gamma {
        found.extend_from_slice(erasures);
    }
    for p in 0..len {
        if found.len() == deg {
            break;
        }
        let x_inv = alpha(g, GF_MAX - p % GF_MAX);
        if poly_eval(g, &lambda, x_inv) == 0 {
            found.push(p);
            if found.len() == deg {
                break;
            }
        }
    }
    if found.len() != deg {
        return None;
    }
    // Forney (first syndrome at α^0): e = X · Ω(X⁻¹) / λ'(X⁻¹),
    // Ω = S·λ mod x^n.
    let s_poly: Vec<u16> = syn.to_vec();
    let mut omega = poly_mul(g, &s_poly, &lambda);
    omega.truncate(n);
    // Formal derivative in characteristic 2: odd terms only, shifted down.
    let dlambda: Vec<u16> = lambda.iter().enumerate().skip(1).map(|(i, &c)| if i % 2 == 1 { c } else { 0 }).collect();
    let mut fixes = Vec::with_capacity(found.len());
    for &p in &found {
        let x = alpha(g, p);
        let x_inv = alpha(g, GF_MAX - p % GF_MAX);
        let num = poly_eval(g, &omega, x_inv);
        let den = poly_eval(g, &dlambda, x_inv);
        if den == 0 {
            return None;
        }
        let e = g.mul(x, g.div(num, den));
        if e != 0 {
            fixes.push((p, e));
        }
    }
    // Check: the corrections must explain every syndrome.
    for (i, &s) in syn.iter().enumerate() {
        let mut v = s;
        for &(p, e) in &fixes {
            v ^= g.mul_exp(e, (i * p) % GF_MAX);
        }
        if v != 0 {
            return None;
        }
    }
    Some(fixes)
}

/// How many errors Berlekamp-Massey needs to explain these syndromes (no
/// erasures): the error locator's degree. Random syndromes give about
/// npar/2; a column with k wrong words gives k (if k < npar/2).
pub fn error_count(syn: &[u16]) -> usize {
    let g = gf();
    let n = syn.len();
    if syn.iter().all(|&s| s == 0) {
        return 0;
    }
    let mut lambda: Vec<u16> = vec![1];
    let mut b: Vec<u16> = vec![1];
    let mut l = 0usize;
    let mut m = 1usize;
    let mut bb: u16 = 1;
    for r in 0..n {
        let mut delta = 0u16;
        for (i, &c) in lambda.iter().enumerate() {
            if i <= r {
                delta ^= g.mul(c, syn[r - i]);
            }
        }
        if delta == 0 {
            m += 1;
            continue;
        }
        let coef = g.div(delta, bb);
        let mut t = lambda.clone();
        if t.len() < b.len() + m {
            t.resize(b.len() + m, 0);
        }
        for (i, &c) in b.iter().enumerate() {
            t[i + m] ^= g.mul(coef, c);
        }
        if 2 * l <= r {
            l = r + 1 - l;
            b = lambda;
            bb = delta;
            m = 1;
        } else {
            m += 1;
        }
        lambda = t;
    }
    l
}

/// The shift (words) at which a damaged disc lines up with an entry: the one
/// where probe columns need the fewest corrections in all (CUETools'
/// FindOffset fallback). None if no shift stands out from random.
pub fn find_shift_damaged(words: &[u16], base: &[u16], theirs: &[u16], npar: usize) -> Option<isize> {
    let probes: Vec<usize> = (0..48).map(|i| (i * 245 + 17) % STRIDE).collect();
    let half = (STRIDE / 2) as isize;
    let mut scores: Vec<(usize, isize)> = Vec::new();
    let mut k = -half + 2;
    while k < half {
        let mut total = 0;
        for &c in &probes {
            let ours = crate::ctdb::shifted_column(words, base, npar, c, k);
            let syn: Vec<u16> = ours.iter().zip(&theirs[c * npar..c * npar + npar]).map(|(a, b)| a ^ b).collect();
            total += error_count(&syn);
        }
        scores.push((total, k));
        k += 2;
    }
    scores.sort();
    let (best, k) = scores[0];
    let random = probes.len() * (npar / 2);
    // Clearly below what random syndromes give, and clearly the best.
    (best * 4 < random * 3 && scores.get(1).is_none_or(|s| s.0 > best + probes.len() / 4)).then_some(k)
}

/// Erasures only: the wrong words are exactly at `erasures` (no unknown
/// errors). Values by Forney, then every syndrome must agree. Cheap: no
/// search over the column.
pub fn decode_erasures_only(syn: &[u16], erasures: &[usize]) -> Option<Vec<(usize, u16)>> {
    let g = gf();
    let n = syn.len();
    if erasures.is_empty() || erasures.len() > n {
        return None;
    }
    let mut gamma: Vec<u16> = vec![1];
    for &p in erasures {
        gamma = poly_mul(g, &gamma, &[1, alpha(g, p)]);
    }
    let mut omega = poly_mul(g, syn, &gamma);
    omega.truncate(n);
    let dgamma: Vec<u16> = gamma.iter().enumerate().skip(1).map(|(i, &c)| if i % 2 == 1 { c } else { 0 }).collect();
    let mut fixes = Vec::with_capacity(erasures.len());
    for &p in erasures {
        let x = alpha(g, p);
        let x_inv = alpha(g, GF_MAX - p % GF_MAX);
        let den = poly_eval(g, &dgamma, x_inv);
        if den == 0 {
            return None;
        }
        let e = g.mul(x, g.div(poly_eval(g, &omega, x_inv), den));
        fixes.push((p, e));
    }
    for (i, &s) in syn.iter().enumerate() {
        let mut v = s;
        for &(p, e) in &fixes {
            v ^= g.mul_exp(e, (i * p) % GF_MAX);
        }
        if v != 0 {
            return None;
        }
    }
    fixes.retain(|&(_, e)| e != 0);
    Some(fixes)
}

/// What [`repair`] did.
#[derive(Debug, Default, Clone)]
pub struct RepairReport {
    /// Columns whose syndromes differed from the database's.
    pub bad_columns: usize,
    /// Of those, repaired.
    pub fixed_columns: usize,
    /// Words changed.
    pub fixed_words: usize,
    /// Columns that couldn't be repaired (too many errors).
    pub failed_columns: usize,
    /// Doubtful words outside the parity's reach (lead-in / lead-out rows).
    pub uncovered_suspects: usize,
}

/// How doubtful a word is (from comparing reads): how many reads agreed
/// with the value voted for (fewer = more doubtful), out of how many.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Doubt {
    pub agree: u8,
    pub reads: u8,
}

impl Doubt {
    /// No clear majority.
    pub fn strong(&self) -> bool {
        (self.agree as usize) * 2 <= self.reads as usize
    }
}

/// Repairs `words` (a whole disc, at the entry's alignment) in place with
/// the database's syndromes `theirs`. `doubts` lists doubtful words (index
/// into `words`) to try as erasures. Per column the most doubtful are tried
/// first: the top npar, then one fewer... down to none; the first decoding
/// that explains every syndrome is used.
pub fn repair(words: &mut [u16], doubts: &[(usize, Doubt)], theirs: &[u16], npar: usize) -> RepairReport {
    repair_with(words, doubts, theirs, npar, 1)
}

/// [`repair`] with the safety margin (`spare` syndromes) explicit.
pub fn repair_with(words: &mut [u16], doubts: &[(usize, Doubt)], theirs: &[u16], npar: usize, spare: usize) -> RepairReport {
    repair_full(words, doubts, &std::collections::HashMap::new(), theirs, npar, spare)
}

/// The full repair. `alts` = for doubtful words, the values the reads saw:
/// a last pass then uses the parity's whole strength (no spare syndromes)
/// but accepts a column only if every corrected word gets one of the values
/// some read actually saw (a wrong decoding gives random values).
pub fn repair_full(
    words: &mut [u16],
    doubts: &[(usize, Doubt)],
    alts: &std::collections::HashMap<usize, Vec<u16>>,
    theirs: &[u16],
    npar: usize,
    spare: usize,
) -> RepairReport {
    let layout = Layout::new((words.len() / 2) as u64);
    let r = layout.stridecount;
    let ours = crate::ctdb::column_syndromes(words, npar);
    let mut report = RepairReport::default();
    // Doubtful positions per column, most doubtful first.
    let mut suspects: Vec<Vec<(Doubt, usize)>> = vec![Vec::new(); STRIDE];
    for &(w, d) in doubts {
        let row = w / STRIDE;
        if row == 0 || row > r {
            report.uncovered_suspects += 1;
            continue;
        }
        suspects[w % STRIDE].push((d, r - row));
    }
    for s in &mut suspects {
        s.sort_by_key(|&(d, _)| (d.agree as u32 * 64 / d.reads.max(1) as u32, std::cmp::Reverse(d.reads)));
    }
    for col in 0..STRIDE {
        let syn: Vec<u16> = (0..npar).map(|i| ours[col * npar + i] ^ theirs[col * npar + i]).collect();
        if syn.iter().all(|&s| s == 0) {
            continue;
        }
        report.bad_columns += 1;
        let ranked: Vec<usize> = suspects[col].iter().map(|&(_, p)| p).collect();
        // Fewest flags first: each try must leave `spare` syndromes to check it.
        let top = ranked.len().min(npar.saturating_sub(spare));
        let fixes = (0..=top).find_map(|m| decode_column_spare(&syn, &ranked[..m], r, spare));
        // Didn't fit with the most doubtful first: search combinations of
        // the column's doubtful words (the wrong ones aren't always the most
        // doubtful). Many tries, so a wider margin: 2 spare syndromes at least.
        // Last: full strength, but every correction must give a value a read saw.
        let read_saw = |fx: &[(usize, u16)]| {
            !fx.is_empty()
                && fx.iter().all(|&(p, e)| {
                    let w = (r - p) * STRIDE + col;
                    alts.get(&w).is_some_and(|v| v.contains(&(words[w] ^ e)))
                })
        };
        let fixes = fixes.or_else(|| {
            let cand: Vec<usize> = ranked.iter().copied().take(14).collect();
            let spare2 = spare.max(2);
            let max_f = cand.len().min(npar.saturating_sub(spare2));
            (1..=max_f).find_map(|f| combinations(cand.len(), f).find_map(|idx| {
                let e: Vec<usize> = idx.iter().map(|&i| cand[i]).collect();
                decode_column_spare(&syn, &e, r, spare2)
            }))
        });
        let fixes = fixes.or_else(|| {
            if alts.is_empty() {
                return None;
            }
            let cand: Vec<usize> = ranked.iter().copied().take(14).collect();
            let max_f = cand.len().min(npar);
            (1..=max_f).find_map(|f| {
                combinations(cand.len(), f).find_map(|idx| {
                    let e: Vec<usize> = idx.iter().map(|&i| cand[i]).collect();
                    decode_erasures_only(&syn, &e).filter(|fx| read_saw(fx))
                })
            })
        });
        match fixes {
            Some(f) => {
                report.fixed_columns += 1;
                for (p, e) in f {
                    let w = (r - p) * STRIDE + col;
                    words[w] ^= e;
                    report.fixed_words += 1;
                }
            }
            None => report.failed_columns += 1,
        }
    }
    report
}

/// All `k`-element index combinations of 0..n, in order.
fn combinations(n: usize, k: usize) -> impl Iterator<Item = Vec<usize>> {
    let mut idx: Vec<usize> = (0..k).collect();
    let mut done = k > n;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let out = idx.clone();
        // Next combination.
        let mut i = k;
        loop {
            if i == 0 {
                done = true;
                break;
            }
            i -= 1;
            if idx[i] < n - k + i {
                idx[i] += 1;
                for j in i + 1..k {
                    idx[j] = idx[j - 1] + 1;
                }
                break;
            }
        }
        Some(out)
    })
}

/// Voted words, the doubtful ones, and the alternatives seen per word.
pub type WordVote = (Vec<u16>, Vec<(usize, Doubt)>, Vec<Vec<u16>>);

/// Votes per 16-bit word over several reads of a track (frames, as
/// `secure_read` gives them): the majority value, and which words were
/// doubtful (`index into the returned words`, strength).
pub fn vote_words(reads: &[Vec<u32>]) -> (Vec<u16>, Vec<(usize, Doubt)>) {
    let (w, d, _) = vote_words_alts(reads);
    (w, d)
}

/// Votes over reads that may have gaps (unreadable stretches, frame ranges
/// per read): only the reads that have a word vote on it. A word no read
/// has is 0 and maximally doubtful (an erasure); a word only one read has
/// is doubtful too.
pub fn vote_words_gaps(reads: &[Vec<u32>], gaps: &[Vec<(usize, usize)>]) -> WordVote {
    if gaps.iter().all(|g| g.is_empty()) {
        return vote_words_alts(reads);
    }
    let n = reads.iter().map(Vec::len).min().unwrap_or(0);
    // Per read: is frame f inside a gap?
    let masks: Vec<Vec<bool>> = (0..reads.len())
        .map(|i| {
            let mut m = vec![false; n];
            for &(a, b) in gaps.get(i).map(Vec::as_slice).unwrap_or(&[]) {
                for x in m.iter_mut().take(b.min(n)).skip(a.min(n)) {
                    *x = true;
                }
            }
            m
        })
        .collect();
    let mut words = Vec::with_capacity(n * 2);
    let mut doubts = Vec::new();
    let mut alts = Vec::new();
    let mut vals: Vec<u16> = Vec::with_capacity(reads.len());
    for f in 0..n {
        for half in 0..2 {
            vals.clear();
            vals.extend(reads.iter().zip(&masks).filter(|(_, m)| !m[f]).map(|(r, _)| (r[f] >> (16 * half)) as u16));
            let idx = words.len();
            if vals.is_empty() {
                words.push(0);
                doubts.push((idx, Doubt { agree: 0, reads: 0 }));
                alts.push(Vec::new());
                continue;
            }
            let first = vals[0];
            if vals.len() > 1 && vals.iter().all(|&v| v == first) {
                words.push(first);
                continue;
            }
            let mut best = (0usize, first);
            for &v in &vals {
                let c = vals.iter().filter(|&&x| x == v).count();
                if c > best.0 {
                    best = (c, v);
                }
            }
            words.push(best.1);
            doubts.push((idx, Doubt { agree: best.0 as u8, reads: vals.len() as u8 }));
            let mut distinct = vals.clone();
            distinct.sort_unstable();
            distinct.dedup();
            alts.push(distinct);
        }
    }
    (words, doubts, alts)
}

/// [`vote_words`], plus each doubtful word's distinct values across the reads.
pub fn vote_words_alts(reads: &[Vec<u32>]) -> WordVote {
    let mut alts = Vec::new();
    let n = reads.iter().map(Vec::len).min().unwrap_or(0);
    let mut words = Vec::with_capacity(n * 2);
    let mut doubts = Vec::new();
    let mut vals: Vec<u16> = Vec::with_capacity(reads.len());
    for f in 0..n {
        for half in 0..2 {
            vals.clear();
            vals.extend(reads.iter().map(|r| (r[f] >> (16 * half)) as u16));
            let first = vals[0];
            if vals.iter().all(|&v| v == first) {
                words.push(first);
                continue;
            }
            // Most common value.
            let mut best = (0usize, first);
            for &v in &vals {
                let c = vals.iter().filter(|&&x| x == v).count();
                if c > best.0 {
                    best = (c, v);
                }
            }
            let idx = words.len();
            words.push(best.1);
            doubts.push((idx, Doubt { agree: best.0 as u8, reads: reads.len() as u8 }));
            let mut distinct = vals.clone();
            distinct.sort_unstable();
            distinct.dedup();
            alts.push(distinct);
        }
    }
    (words, doubts, alts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctdb::column_syndromes;

    fn disc(rows: usize) -> Vec<u16> {
        (0..STRIDE * rows + 2000).map(|i| ((i as u64 * 2654435761) >> 9) as u16).collect()
    }

    #[test]
    fn decodes_errors_and_erasures_in_one_column() {
        let g = gf();
        let len = 500;
        let npar = 8;
        // Syndromes of an error pattern: e at positions.
        let errs = [(3usize, 0x1234u16), (77, 0x0f0f), (420, 0x8001)];
        let syn: Vec<u16> = (0..npar)
            .map(|i| errs.iter().fold(0u16, |acc, &(p, e)| acc ^ g.mul_exp(e, (i * p) % GF_MAX)))
            .collect();
        // 3 unknown errors: 6 <= 8.
        let mut f = decode_column(&syn, &[], len).unwrap();
        f.sort();
        assert_eq!(f, errs.to_vec());
        // Two of them as erasures, plus a false alarm: 2*1 + 3 <= 8.
        let mut f = decode_column(&syn, &[3, 77, 200], len).unwrap();
        f.sort();
        assert_eq!(f, errs.to_vec());
        // Five unknown errors don't fit (10 > 8): refused, not miscorrected.
        let more = [(1usize, 5u16), (2, 6), (3, 7), (4, 8), (5, 9)];
        let syn5: Vec<u16> = (0..npar).map(|i| more.iter().fold(0u16, |a, &(p, e)| a ^ g.mul_exp(e, (i * p) % GF_MAX))).collect();
        let r = decode_column(&syn5, &[], len);
        assert!(r.is_none_or(|f| f.len() <= 4));
        // But all five as erasures fit (5 <= 8).
        let mut f = decode_column(&syn5, &[1, 2, 3, 4, 5], len).unwrap();
        f.sort();
        assert_eq!(f, more.to_vec());
    }

    #[test]
    fn repairs_a_disc() {
        let good = disc(60);
        let npar = 8;
        let theirs = column_syndromes(&good, npar);
        let mut bad = good.clone();
        let mut doubts = Vec::new();
        // Column 100: 7 damaged words, all flagged (fits only as erasures).
        for k in 0..7 {
            let w = (5 + k * 6) * STRIDE + 100;
            bad[w] ^= 0x5a5a;
            doubts.push((w, Doubt { agree: 1, reads: 8 }));
        }
        // Column 2000: 3 unflagged errors.
        for k in 0..3 {
            bad[(10 + k * 9) * STRIDE + 2000] ^= 0x1111;
        }
        // Column 3000: 2 weakly flagged + 2 unflagged = 2 + 4 = 6.
        for k in 0..4 {
            let w = (20 + k) * STRIDE + 3000;
            bad[w] ^= 0x00ff;
            if k < 2 {
                doubts.push((w, Doubt { agree: 6, reads: 8 }));
            }
        }
        doubts.sort();
        let rep = repair(&mut bad, &doubts, &theirs, npar);
        assert_eq!(rep.failed_columns, 0, "{rep:?}");
        assert_eq!(rep.bad_columns, 3);
        assert_eq!(bad, good);
    }

    #[test]
    fn finds_the_shift_of_a_damaged_disc() {
        let good = disc(80);
        let npar = 8;
        let k: isize = -2468;
        // Their disc: ours shifted (their word p = our word p + k).
        let theirs_disc: Vec<u16> = (0..good.len() as isize).map(|p| good[(p + k).clamp(0, good.len() as isize - 1) as usize]).collect();
        let theirs = column_syndromes(&theirs_disc, npar);
        // Our copy: damaged in every column (2 words each).
        let mut ours = good.clone();
        for c in 0..STRIDE {
            ours[(5 + c % 30) * STRIDE + c] ^= 0x1357;
            ours[(40 + c % 20) * STRIDE + c] ^= 0x2468;
        }
        let base = column_syndromes(&ours, npar);
        assert_eq!(crate::ctdb::find_shift(&ours, &base, &theirs, npar), None, "no column is exact");
        assert_eq!(find_shift_damaged(&ours, &base, &theirs, npar), Some(k));
    }

    #[test]
    fn combinations_in_order() {
        let c: Vec<Vec<usize>> = combinations(4, 2).collect();
        assert_eq!(c, vec![vec![0, 1], vec![0, 2], vec![0, 3], vec![1, 2], vec![1, 3], vec![2, 3]]);
        assert_eq!(combinations(3, 0).count(), 1);
        assert_eq!(combinations(2, 3).count(), 0);
    }

    #[test]
    fn finds_the_right_flags_among_many() {
        // 4 wrong words, all doubted, among 10 doubted words, the wrong ones
        // ranked last: the top-N tries fail, the combination search finds them.
        let good = disc(60);
        let npar = 8;
        let theirs = column_syndromes(&good, npar);
        let mut bad = good.clone();
        let col = 777;
        let mut doubts = Vec::new();
        for k in 0..10 {
            let w = (3 + k * 5) * STRIDE + col;
            let wrong = k >= 6;
            if wrong {
                bad[w] ^= 0x3c3c;
            }
            // Correct words look MORE doubtful than the wrong ones.
            doubts.push((w, Doubt { agree: if wrong { 6 } else { 4 }, reads: 8 }));
        }
        let rep = repair_with(&mut bad, &doubts, &theirs, npar, 1);
        assert_eq!(rep.failed_columns, 0, "{rep:?}");
        assert_eq!(bad, good);
    }

    #[test]
    fn voting_skips_gaps() {
        // Three reads; read 0 has a gap over frame 1, read 1 over frames 1-2.
        let reads = vec![vec![10u32, 99, 30], vec![10, 98, 97], vec![10, 20, 30]];
        let gaps = vec![vec![(1, 2)], vec![(1, 3)], vec![]];
        let (w, d, _) = vote_words_gaps(&reads, &gaps);
        // Frame 1: only read 2 has it -> its value, doubtful (one read).
        assert_eq!(w[2], 20);
        assert!(d.iter().any(|&(i, x)| i == 2 && x.reads == 1));
        // Frame 2: reads 0 and 2 agree on 30 -> no doubt.
        assert_eq!(w[4], 30);
        assert!(!d.iter().any(|&(i, _)| i == 4));
        // A frame every read misses: 0, erasure.
        let (w, d, _) = vote_words_gaps(&[vec![5u32], vec![6]], &[vec![(0, 1)], vec![(0, 1)]]);
        assert_eq!(w[0], 0);
        assert_eq!(d[0].1, Doubt { agree: 0, reads: 0 });
    }

    #[test]
    fn voting_marks_doubt() {
        let reads = vec![vec![0x0001_0002u32, 7], vec![0x0001_0002, 8], vec![0x0001_0003, 9]];
        let (w, d) = vote_words(&reads);
        assert_eq!(w[0], 2);
        assert_eq!(w[1], 1);
        assert_eq!(d[0], (0, Doubt { agree: 2, reads: 3 }));
        assert!(!d[0].1.strong());
        assert_eq!(d[1], (2, Doubt { agree: 1, reads: 3 }));
        assert!(d[1].1.strong());
    }
}
