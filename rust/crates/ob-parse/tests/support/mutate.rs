//! Mutations of real recorded frames, shared by the venue differential tests:
//! structure-preserving edits (digit-for-digit changes, valid field insertion) that keep
//! many messages valid, and destructive ones (byte edits, deletions, duplications, JSON
//! fragments) that produce subtly invalid input.

use proptest::prelude::*;

/// Fragments that are likely to produce interesting (often still-valid) JSON when
/// spliced into a real frame.
pub(crate) const FRAGMENTS: &[&str] = &[
    ",",
    ":",
    "[",
    "]",
    "{",
    "}",
    "\"",
    " ",
    "\n",
    "0",
    "1",
    "-",
    ".",
    "e",
    "null",
    "true",
    r#","x":1"#,
    r#","x":[1,{"y":null}]"#,
    r#","b":[]"#,
    r#","u":1"#,
    r#""E":"#,
    r#"["1.0","1.000"]"#,
    r#""58631.95""#,
    r#""0.0001""#,
    r#""-1""#,
    "1e3",
    "01",
    "18446744073709551616",
];

/// Valid fields to insert (unknown keys, or known ones that are often absent).
pub(crate) const FIELDS: &[&str] = &[
    r#""x":1,"#,
    r#""ps":"BTCUSDT","#,
    r#""st":1,"#,
    r#""zz":[1,{"y":null,"w":[true,false]}],"#,
    r#" "n" : -0.5e-7 , "#,
    r#""result":null,"#,
    r#""q":"",  "#,
];

#[derive(Clone, Debug)]
pub(crate) enum Mutation {
    /// Replace the first digit at or after a position with another digit (keeps the JSON
    /// valid; may push a price off-tick or change an ID).
    Digit(usize, u8),
    /// Insert a valid `"key":value,` field right after the opening brace.
    InsertField(usize),
    Replace(usize, u8),
    Delete(usize, usize),
    Duplicate(usize, usize),
    Insert(usize, usize),
}

pub(crate) fn mutation() -> impl Strategy<Value = Mutation> {
    let byte = prop::sample::select(b"0123456789.-,:[]{}\" eExntrlu".to_vec());
    prop_oneof![
        3 => (any::<usize>(), b'0'..=b'9').prop_map(|(i, d)| Mutation::Digit(i, d)),
        3 => (0..FIELDS.len()).prop_map(Mutation::InsertField),
        1 => (any::<usize>(), byte).prop_map(|(i, b)| Mutation::Replace(i, b)),
        1 => (any::<usize>(), 1_usize..20).prop_map(|(i, n)| Mutation::Delete(i, n)),
        1 => (any::<usize>(), 1_usize..40).prop_map(|(i, n)| Mutation::Duplicate(i, n)),
        1 => (any::<usize>(), 0..FRAGMENTS.len()).prop_map(|(i, f)| Mutation::Insert(i, f)),
    ]
}

pub(crate) fn mutate(frame: &[u8], muts: &[Mutation]) -> Vec<u8> {
    let mut v = frame.to_vec();
    for m in muts {
        let len = v.len().max(1);
        match *m {
            Mutation::Digit(i, d) => {
                if let Some(pos) = (i % len..v.len()).find(|&p| v[p].is_ascii_digit()) {
                    v[pos] = d;
                }
            }
            Mutation::InsertField(f) => {
                if let Some(pos) = v.iter().position(|&b| b == b'{') {
                    v.splice(pos + 1..pos + 1, FIELDS[f].bytes());
                }
            }
            Mutation::Replace(i, b) => {
                if !v.is_empty() {
                    v[i % len] = b;
                }
            }
            Mutation::Delete(i, n) => {
                let s = i % len;
                let e = (s + n).min(v.len());
                v.drain(s.min(v.len())..e);
            }
            Mutation::Duplicate(i, n) => {
                let s = (i % len).min(v.len());
                let e = (s + n).min(v.len());
                let chunk = v[s..e].to_vec();
                v.splice(e..e, chunk);
            }
            Mutation::Insert(i, f) => {
                let at = (i % len).min(v.len());
                v.splice(at..at, FRAGMENTS[f].bytes());
            }
        }
    }
    v
}
