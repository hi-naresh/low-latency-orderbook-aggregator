//! Differential tests: the fast Bybit parser must reach exactly the same conclusion as an
//! independent serde_json implementation of the same rules
//! (`tests/support/bybit_oracle.rs`), on real frames, mutated real frames, generated
//! messages (shuffled top-level *and* `data` fields, swapped and unknown fields), and
//! arbitrary bytes. Never panicking is checked on every input.
#![allow(clippy::unwrap_used)]

#[path = "support/comparable.rs"]
mod comparable;
#[path = "support/mutate.rs"]
mod mutate;
#[path = "support/bybit_oracle.rs"]
mod oracle;

use comparable::comparable;
use mutate::{FIELDS, mutate, mutation};
use ob_core::InstrumentSpec;
use oracle::{OracleResult, fast_with_top, oracle_with_top};
use proptest::prelude::*;

const FIXTURE: &str = include_str!("fixtures/bybit_orderbook50.jsonl");
const TOP_FIXTURE: &str = include_str!("fixtures/bybit_orderbook1.jsonl");
const TOPIC: &str = "orderbook.50.BTCUSDT";
/// The top-of-book topic (step 18): accepted alongside `TOPIC`.
const TOP_TOPIC: &str = "orderbook.1.BTCUSDT";

fn fast(msg: &[u8], spec: &InstrumentSpec, topic: &str, symbol: &str) -> OracleResult {
    fast_with_top(msg, spec, topic, Some(TOP_TOPIC), symbol)
}
fn reference(msg: &[u8], spec: &InstrumentSpec, topic: &str, symbol: &str) -> OracleResult {
    oracle_with_top(msg, spec, topic, Some(TOP_TOPIC), symbol)
}
/// Recorded frame `i` of either fixture (depth first, then top-of-book).
fn recorded(i: usize) -> &'static [u8] {
    FIXTURE
        .lines()
        .chain(TOP_FIXTURE.lines())
        .nth(i)
        .unwrap()
        .as_bytes()
}

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

fn check(msg: &[u8]) -> Result<(), TestCaseError> {
    let got = fast(msg, &spec(), TOPIC, "BTCUSDT"); // must not panic
    if comparable(msg) {
        let want = reference(msg, &spec(), TOPIC, "BTCUSDT");
        prop_assert_eq!(got, want, "input: {}", String::from_utf8_lossy(msg));
    }
    Ok(())
}

#[test]
fn recorded_frames_agree() {
    for (i, frame) in FIXTURE.lines().chain(TOP_FIXTURE.lines()).enumerate() {
        let want = reference(frame.as_bytes(), &spec(), TOPIC, "BTCUSDT");
        assert_ne!(want, OracleResult::Reject);
        if i >= 60 {
            assert!(
                matches!(want, OracleResult::Book { top: true, .. }),
                "{want:?}"
            );
        }
        assert_eq!(fast(frame.as_bytes(), &spec(), TOPIC, "BTCUSDT"), want);
    }
}

/// A Bybit message built from shuffled parts: a complete valid book message (sometimes
/// with one field swapped for a random variant, plus unknown fields), a response, or a
/// random set of fields.
fn generated() -> impl Strategy<Value = Vec<u8>> {
    let int = prop_oneof![
        (0_u64..2_000_000_000_000).prop_map(|n| n.to_string()),
        Just("1".to_string()),
        Just("01".to_string()),
        Just("-1".to_string()),
        Just("2.5".to_string()),
        Just("\"7\"".to_string()),
    ];
    let decimal = prop_oneof![
        (0_u32..1_000_000, 0_u32..10).prop_map(|(i, f)| format!("\"{i}.{f}0\"")),
        (0_u32..1_000_000, 0_u32..100).prop_map(|(i, f)| format!("\"{i}.{f:02}\"")),
        (0_u32..10_000, 0_u32..1000).prop_map(|(i, f)| format!("\"{i}.{f:03}\"")),
        Just("\"0\"".to_string()),
        Just("\"-1\"".to_string()),
        Just("\"x\"".to_string()),
    ];
    let level = (decimal.clone(), decimal).prop_map(|(p, q)| format!("[{p},{q}]"));
    let levels = prop::collection::vec(level, 0..5).prop_map(|v| format!("[{}]", v.join(",")));
    let string = prop::sample::select(vec![
        "\"snapshot\"",
        "\"delta\"",
        "\"update\"",
        "\"BTCUSDT\"",
        "\"ETHUSDT\"",
        "\"orderbook.50.BTCUSDT\"",
        "\"orderbook.1.BTCUSDT\"",
        "\"subscribe\"",
        "\"ping\"",
        "\"\"",
    ])
    .prop_map(String::from);
    let boolean =
        prop::sample::select(vec!["true", "false", "1", "\"true\"", "null"]).prop_map(String::from);
    let data_field = prop_oneof![
        string.clone().prop_map(|v| format!("\"s\":{v}")),
        levels.clone().prop_map(|v| format!("\"b\":{v}")),
        levels.prop_map(|v| format!("\"a\":{v}")),
        int.clone().prop_map(|v| format!("\"u\":{v}")),
        int.clone().prop_map(|v| format!("\"seq\":{v}")),
        Just("\"x\":[1,{\"y\":null}]".to_string()),
    ];
    let top_field = prop_oneof![
        string.clone().prop_map(|v| format!("\"topic\":{v}")),
        string.clone().prop_map(|v| format!("\"type\":{v}")),
        int.clone().prop_map(|v| format!("\"ts\":{v}")),
        int.prop_map(|v| format!("\"cts\":{v}")),
        boolean.prop_map(|v| format!("\"success\":{v}")),
        string.clone().prop_map(|v| format!("\"op\":{v}")),
        string.prop_map(|v| format!("\"ret_msg\":{v}")),
        prop::collection::vec(data_field.clone(), 0..6)
            .prop_map(|v| format!("\"data\":{{{}}}", v.join(","))),
        Just("\"data\":[]".to_string()),
    ];
    let unknown = prop::sample::select(FIELDS.to_vec())
        .prop_map(|f| f.trim_end().trim_end_matches(',').to_string());
    let ws = prop::sample::select(vec!["", "", " ", "\n"]);

    let data_base: Vec<String> = [
        r#""s":"BTCUSDT""#,
        r#""b":[["58656.20","0.835"],["58656.10","0.002"]]"#,
        r#""a":[["58656.30","0"]]"#,
        r#""u":22042291"#,
        r#""seq":855165474295"#,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let data = (
        Just(data_base).prop_shuffle(),
        prop::option::weighted(0.25, (0_usize..5, data_field)),
    )
        .prop_map(|(mut f, swap)| {
            if let Some((i, v)) = swap {
                f[i] = v;
            }
            format!("\"data\":{{{}}}", f.join(","))
        });
    let complete = (
        data,
        prop::sample::select(vec!["\"snapshot\"", "\"delta\""]),
        any::<bool>(),
        prop::collection::vec(unknown.clone(), 0..3),
        prop::option::weighted(0.25, top_field.clone()),
        ws.clone(),
        prop::sample::select(vec![TOPIC, TOPIC, TOP_TOPIC]),
    )
        .prop_flat_map(|(data, kind, cts, extra, swap, w, topic)| {
            let mut fields = vec![
                format!("\"topic\":\"{topic}\""),
                format!("\"type\":{kind}"),
                "\"ts\":1795610797394".to_string(),
                data,
            ];
            if cts {
                fields.push("\"cts\":1795610797392".to_string());
            }
            if let Some(f) = swap {
                fields[0] = f; // replace topic with something random (often valid again)
            }
            fields.extend(extra);
            Just(fields)
                .prop_shuffle()
                .prop_map(move |f| format!("{{{w}{}{w}}}", f.join(&format!("{w},{w}"))))
        });
    let response = (
        prop::sample::select(vec!["subscribe", "unsubscribe", "ping", "pong", "auth"]),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(op, ok, with_success)| {
            let success = if with_success {
                format!("\"success\":{ok},")
            } else {
                String::new()
            };
            format!(r#"{{{success}"ret_msg":"","conn_id":"abc","req_id":"depth","op":"{op}"}}"#)
        });
    let random = prop::collection::vec((top_field, ws), 0..10).prop_map(|f| {
        let parts: Vec<String> = f.into_iter().map(|(f, w)| format!("{w}{f}{w}")).collect();
        format!("{{{}}}", parts.join(","))
    });
    prop_oneof![4 => complete, 1 => response, 2 => random].prop_map(String::into_bytes)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 20_000, ..ProptestConfig::default() })]

    #[test]
    fn mutated_real_frames_agree(frame in 0_usize..120, muts in prop::collection::vec(mutation(), 1..4)) {
        check(&mutate(recorded(frame), &muts))?;
    }

    #[test]
    fn generated_messages_agree(msg in generated()) {
        check(&msg)?;
    }

    #[test]
    fn arbitrary_bytes_never_panic(msg in prop::collection::vec(any::<u8>(), 0..200)) {
        check(&msg)?;
    }
}

/// Each generator must produce plenty of accepted *and* rejected input, or the
/// agreement above proves little (see step 6 notes).
#[test]
fn generators_exercise_both_accept_and_reject() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let mut rate = |strategy: &dyn Fn(&mut TestRunner) -> Vec<u8>| {
        let n = 2_000;
        let accepted = (0..n)
            .filter(|_| {
                reference(&strategy(&mut runner), &spec(), TOPIC, "BTCUSDT") != OracleResult::Reject
            })
            .count();
        accepted as f64 / f64::from(n)
    };
    let mutated = rate(&|r| {
        let (f, m) = (0_usize..120, prop::collection::vec(mutation(), 1..4))
            .new_tree(r)
            .unwrap()
            .current();
        mutate(recorded(f), &m)
    });
    let gen_rate = rate(&|r| generated().new_tree(r).unwrap().current());
    eprintln!(
        "accepted: mutated {:.0}%, generated {:.0}%",
        mutated * 100.0,
        gen_rate * 100.0
    );
    // Messages on both topics must be accepted sometimes, or one path is untested.
    let tops = (0..2_000)
        .filter(|_| {
            matches!(
                reference(
                    &generated().new_tree(&mut runner).unwrap().current(),
                    &spec(),
                    TOPIC,
                    "BTCUSDT"
                ),
                OracleResult::Book { top: true, .. }
            )
        })
        .count();
    eprintln!("generated top-of-book messages accepted: {tops} / 2000");
    assert!(
        tops > 20,
        "the generator must produce valid top-of-book messages"
    );
    for r in [mutated, gen_rate] {
        assert!(
            (0.10..=0.90).contains(&r),
            "acceptance rate {r} too lopsided to test both paths"
        );
    }
}
