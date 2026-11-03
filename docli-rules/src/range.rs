// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The range grammar for reading part of a note or of a file's extracted text (v0.48.0 D12): `40-80`,
//! `40-` (to the end) or a bare `40` (one), 1-based and inclusive. `docli read --lines` and the MCP
//! read tools' `lines` / `pages` arguments parse it here, so the CLI and the api can never disagree
//! about what a range means.

use std::fmt;

/// A parsed range. `to: None` = to the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub from: usize,
    pub to: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    /// Not `A-B`, `A-` or `A`.
    NotARange,
    /// Numbering starts at 1.
    StartsAtZero,
    /// `B < A`.
    Backwards { from: usize, to: usize },
}

/// Parse `A-B`, `A-` or `A` (surrounding whitespace ignored).
pub fn parse_range(spec: &str) -> Result<Range, RangeError> {
    let num = |s: &str| -> Result<usize, RangeError> {
        match s.trim().parse::<usize>() {
            Ok(0) => Err(RangeError::StartsAtZero),
            Ok(n) => Ok(n),
            Err(_) => Err(RangeError::NotARange),
        }
    };
    let spec = spec.trim();
    match spec.split_once('-') {
        None => {
            let a = num(spec)?;
            Ok(Range {
                from: a,
                to: Some(a),
            })
        }
        Some((a, "")) => Ok(Range {
            from: num(a)?,
            to: None,
        }),
        Some((a, b)) => {
            let (from, to) = (num(a)?, num(b)?);
            if to < from {
                return Err(RangeError::Backwards { from, to });
            }
            Ok(Range { from, to: Some(to) })
        }
    }
}

/// The canonical spelling — what `parse_range` reads back to the same range.
impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to {
            Some(to) if to == self.from => write!(f, "{}", self.from),
            Some(to) => write!(f, "{}-{to}", self.from),
            None => write!(f, "{}-", self.from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_shapes_and_their_round_trip() {
        for (spec, from, to) in [
            ("40-80", 40, Some(80)),
            ("40-", 40, None),
            ("40", 40, Some(40)),
        ] {
            let r = parse_range(spec).unwrap();
            assert_eq!((r.from, r.to), (from, to), "{spec}");
            assert_eq!(parse_range(&r.to_string()).unwrap(), r);
        }
        assert_eq!(
            parse_range(" 3 - 5 ").unwrap(),
            Range {
                from: 3,
                to: Some(5)
            }
        );
    }

    #[test]
    fn malformed_ranges_say_why() {
        assert_eq!(parse_range("0-4"), Err(RangeError::StartsAtZero));
        assert_eq!(
            parse_range("9-3"),
            Err(RangeError::Backwards { from: 9, to: 3 })
        );
        for bad in ["", "-", "-5", "a-b", "1-2-3", "1.5"] {
            assert_eq!(parse_range(bad), Err(RangeError::NotARange), "{bad:?}");
        }
    }
}
