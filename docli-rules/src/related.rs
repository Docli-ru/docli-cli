// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The ONE function `related` shares across two release trains (v0.29.9 D5/D7): cosine over two
//! sparse term vectors.
//!
//! The server's lexical arm is DEFINED as cosine over the per-note vectors the indexer emits, and
//! the CLI evaluates the same vectors offline. Exactness between the two ends rests on them
//! running the same function over the same bytes — so the function lives here, in the crate both
//! sides depend on, and takes the vectors in the one representation the artifact stores:
//! `(dictionary index, f32 weight)` pairs **sorted by index ascending**.
//!
//! Two f32 sums agree only if they add the same terms in the same order, which is why the
//! iteration order is part of the contract rather than an implementation detail: dot product and
//! both norms are accumulated in ascending-index order, in `f32`, with no fused multiply-add and
//! no reassociation. A vector that is not sorted by index is a caller bug, not an input this
//! function tolerates — the merge join would silently miss shared terms.
//!
//! No `uuid`, no kind type, no dependency: this crate ships in the public MIT mirror and stays
//! dep-thin (`ci/run-checks.sh` pins its composition).

/// Cosine similarity of two sparse vectors, each `(dict_idx, weight)` sorted by `dict_idx`
/// ascending. Returns `0.0` when either vector is empty or has a zero norm (no shared term can
/// score, and a division by zero would otherwise yield `NaN`).
pub fn cosine(a: &[(u32, f32)], b: &[(u32, f32)]) -> f32 {
    let mut dot = 0.0f32;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        let (ia, wa) = a[i];
        let (ib, wb) = b[j];
        match ia.cmp(&ib) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                dot += wa * wb;
                i += 1;
                j += 1;
            }
        }
    }
    if dot == 0.0 {
        return 0.0;
    }
    let na = norm(a);
    let nb = norm(b);
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

/// The Euclidean norm, accumulated in index order in `f32` — the same discipline as the dot.
fn norm(v: &[(u32, f32)]) -> f32 {
    let mut s = 0.0f32;
    for (_, w) in v {
        s += w * w;
    }
    s.sqrt()
}

/// The three shared terms with the largest weight PRODUCT — `why.terms` on both ends (D3).
/// Ties break by dictionary index ascending, so two implementations over one artifact list the
/// same three. Returns dictionary indices; the caller maps them to lexemes.
pub fn top_shared_terms(a: &[(u32, f32)], b: &[(u32, f32)], cap: usize) -> Vec<u32> {
    let mut shared: Vec<(f32, u32)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        let (ia, wa) = a[i];
        let (ib, wb) = b[j];
        match ia.cmp(&ib) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared.push((wa * wb, ia));
                i += 1;
                j += 1;
            }
        }
    }
    shared.sort_by(|x, y| {
        y.0.partial_cmp(&x.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.1.cmp(&y.1))
    });
    shared.truncate(cap);
    shared.into_iter().map(|(_, idx)| idx).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_vectors_score_one_and_disjoint_ones_zero() {
        let a = [(1, 0.5f32), (4, 2.0), (9, 1.0)];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        let b = [(2, 1.0f32), (3, 1.0)];
        assert_eq!(cosine(&a, &b), 0.0);
        assert_eq!(cosine(&a, &[]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_is_symmetric_and_scale_invariant() {
        let a = [(1, 1.0f32), (2, 2.0), (5, 0.5)];
        let b = [(2, 1.0f32), (5, 3.0), (7, 1.0)];
        let ab = cosine(&a, &b);
        assert!(ab > 0.0 && ab < 1.0);
        assert_eq!(
            ab.to_bits(),
            cosine(&b, &a).to_bits(),
            "symmetric to the bit"
        );
        let b2: Vec<(u32, f32)> = b.iter().map(|(i, w)| (*i, w * 4.0)).collect();
        assert!((cosine(&a, &b2) - ab).abs() < 1e-6);
    }

    #[test]
    fn a_zero_weight_vector_never_yields_nan() {
        let a = [(1, 0.0f32)];
        let b = [(1, 1.0f32)];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    #[test]
    fn top_shared_terms_orders_by_product_then_index() {
        let a = [(1, 1.0f32), (2, 3.0), (3, 2.0), (4, 2.0), (5, 0.1)];
        let b = [(1, 1.0f32), (2, 1.0), (3, 1.5), (4, 1.5), (9, 9.0)];
        // Products: 1→1, 2→3, 3→3, 4→3, 5 absent from b. Ties (2,3,4 at 3.0) break by index.
        assert_eq!(top_shared_terms(&a, &b, 3), vec![2, 3, 4]);
        assert_eq!(top_shared_terms(&a, &b, 10), vec![2, 3, 4, 1]);
        assert!(top_shared_terms(&a, &[], 3).is_empty());
    }
}
