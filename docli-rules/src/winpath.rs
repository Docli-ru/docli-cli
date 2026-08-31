// SPDX-FileCopyrightText: 2026 OOO Agitek
// SPDX-License-Identifier: MIT

//! The Windows path projection — the Rust twin of the plugin's `winPath.ts` (v0.27.1 D8), ported
//! line-for-line and pinned by the shared vector file `vectors/winpath.json` (the plugin-side test
//! consumes the same file; the two arms ship on DIFFERENT release trains, so the vectors are the
//! contract).
//!
//! Design recap (see `winPath.ts` for the full argument): the mapping optimizes for
//! `encode(decode(local)) == local` for EVERY existing local name (no echo-phantom) by making
//! `encode` IDENTITY on every name that is legal on Windows, and escaping ONLY names that
//! genuinely need a mapping. Accepted residual, and the reason the CLI adds a park rule on top
//! (v0.28.0 D3): the projection is not injective across domains — a literal local `a%3Cb.md` and
//! a server `a<b.md` project onto one local name. The CLI GUARD-PARKS such collisions (and `%XX`
//! expansions past filesystem length limits) after projection; the encoding itself never widens.

/// The characters Windows forbids in a file name (`\` included — a path separator can't appear in
/// a local segment, but a server segment may legally contain one).
fn is_illegal_char(ch: char) -> bool {
    matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') || (ch as u32) < 0x20
}

/// `^(CON|PRN|AUX|NUL|COM[1-9¹²³]|LPT[1-9¹²³])(\.|$)` case-insensitively, hand-rolled (no
/// regex dep). The superscript digits (U+00B9/U+00B2/U+00B3) are reserved by Windows exactly
/// like their ASCII forms (Codex round 28).
fn is_device_name(seg: &str) -> bool {
    let bytes = seg.as_bytes();
    let prefix_len = |p: &str| -> Option<usize> {
        // Byte-wise: a multibyte segment must not be sliced at a non-char boundary, and the
        // device prefixes are ASCII, so ASCII-case-insensitive byte comparison is the JS regex's
        // exact semantics.
        if bytes.len() >= p.len() && bytes[..p.len()].eq_ignore_ascii_case(p.as_bytes()) {
            Some(p.len())
        } else {
            None
        }
    };
    let after = ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .find_map(|p| prefix_len(p))
        .or_else(|| {
            ["COM", "LPT"].iter().find_map(|p| {
                let n = prefix_len(p)?;
                match bytes.get(n) {
                    Some(d) if (b'1'..=b'9').contains(d) => Some(n + 1),
                    // ¹ ² ³ are two UTF-8 bytes: C2 B9 / C2 B2 / C2 B3.
                    Some(0xC2) if matches!(bytes.get(n + 1), Some(0xB9 | 0xB2 | 0xB3)) => {
                        Some(n + 2)
                    }
                    _ => None,
                }
            })
        });
    match after {
        Some(n) => n == seg.len() || bytes.get(n) == Some(&b'.'),
        None => false,
    }
}

fn hex(ch: char) -> String {
    format!("%{:02X}", ch as u32)
}

/// Does this SERVER name need a mapping at all? Identity otherwise — which is what keeps every
/// legal local name (bare `%` included) a fixed point of encode.
fn needs_escape(seg: &str) -> bool {
    seg.chars().any(is_illegal_char)
        || is_device_name(seg)
        || seg.ends_with('.')
        || seg.ends_with(' ')
}

/// The core escaper — applied ONLY to names [`needs_escape`] admits. Escapes every illegal/control
/// character, every literal `%` (the escape-the-escape rule that keeps it injective on its
/// domain), the first character of a reserved device name, and a trailing dot/space.
fn core_escape(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for ch in seg.chars() {
        if is_illegal_char(ch) || ch == '%' {
            out.push_str(&hex(ch));
        } else {
            out.push(ch);
        }
    }
    if is_device_name(&out) {
        let first = out.chars().next().expect("device names are non-empty");
        out = format!("{}{}", hex(first), &out[first.len_utf8()..]);
    }
    if out.ends_with('.') || out.ends_with(' ') {
        let last = out.pop().expect("non-empty by the trailing check");
        out.push_str(&hex(last));
    }
    out
}

/// Encode ONE server name segment into its Windows-safe local spelling.
pub fn encode_win_segment(seg: &str) -> String {
    if needs_escape(seg) {
        core_escape(seg)
    } else {
        seg.to_string()
    }
}

/// Decode a local segment back to its server spelling — accepting ONLY the exact image of
/// [`encode_win_segment`]: permissively decode a candidate and take it iff the candidate genuinely
/// needed escaping AND re-encoding reproduces the local segment byte-for-byte. Everything else —
/// bare `%`, `%20`-style browser names, `%25` literals — passes through verbatim.
pub fn decode_win_segment(seg: &str) -> String {
    if !seg.contains('%') {
        return seg.to_string();
    }
    let candidate = permissive_decode(seg);
    if candidate == seg {
        return seg.to_string();
    }
    if needs_escape(&candidate) && core_escape(&candidate) == seg {
        candidate
    } else {
        seg.to_string()
    }
}

/// The permissive `%XX` decoder (JS `String.fromCharCode(parseInt(h, 16))` produces the Latin-1
/// scalar; `char::from(u8)` is the same mapping).
fn permissive_decode(seg: &str) -> String {
    let chars: Vec<char> = seg.chars().collect();
    let mut out = String::with_capacity(seg.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' && i + 2 < chars.len() + 1 {
            let h: String = chars[i + 1..(i + 3).min(chars.len())].iter().collect();
            if h.len() == 2 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                let byte = u8::from_str_radix(&h, 16).expect("two hex digits");
                out.push(char::from(byte));
                i += 3;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Encode a whole SERVER path into its local spelling (per segment).
pub fn encode_win_path(path: &str) -> String {
    path.split('/')
        .map(encode_win_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Decode a whole LOCAL path back to its server spelling (per segment).
pub fn decode_win_path(path: &str) -> String {
    path.split('/')
        .map(decode_win_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Does this server path need a mapping at all (fast common-case check)?
pub fn needs_win_mapping(path: &str) -> bool {
    path.split('/').any(needs_escape)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Vectors {
        /// `encode(server) == local` AND `decode(local) == server`.
        roundtrip: Vec<Pair>,
        /// Decode-only expectations (projection collisions, passthroughs).
        decode: Vec<Pair>,
    }
    #[derive(serde::Deserialize)]
    struct Pair {
        server: String,
        local: String,
    }

    /// The cross-train pin: the plugin-side test (`apps/obsidian-plugin`) consumes the SAME file
    /// against `winPath.ts`.
    #[test]
    fn winpath_agrees_with_the_shared_vectors() {
        let v: Vectors = serde_json::from_str(include_str!("../vectors/winpath.json")).unwrap();
        assert!(
            v.roundtrip.len() >= 10,
            "the winpath vector file has thinned out"
        );
        for p in &v.roundtrip {
            assert_eq!(encode_win_path(&p.server), p.local, "encode {:?}", p.server);
            assert_eq!(decode_win_path(&p.local), p.server, "decode {:?}", p.local);
        }
        for p in &v.decode {
            assert_eq!(
                decode_win_path(&p.local),
                p.server,
                "decode-only {:?}",
                p.local
            );
        }
    }

    /// The no-echo-phantom invariant, on this side of the twin too: encode is identity on the
    /// decode image of every legal local name.
    #[test]
    fn encode_decode_local_is_identity_on_legal_names() {
        for local in [
            "a%b.md",
            "%20name.md",
            "%25.md",
            "plain.md",
            "Кэш-политика.md",
        ] {
            assert_eq!(
                encode_win_segment(&decode_win_segment(local)),
                local,
                "{local}"
            );
        }
    }
}
