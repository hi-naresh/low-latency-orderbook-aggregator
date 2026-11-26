//! Differential tests: the fast Binance parser must reach exactly the same conclusion as
//! an independent serde_json implementation of the same rules
//! (`tests/support/binance_oracle.rs`): same accept/reject, and for accepted messages the
//! same fields and levels.
//!
//! Inputs:
//! 1. every recorded real frame;
//! 2. recorded frames with random **mutations** (byte edits, deletions, duplications,
//!    insertions of JSON fragments), which produce a mix of valid and subtly invalid JSON;
//! 3. **generated** messages: random field order, missing and duplicate fields, unknown
//!    fields with random JSON values, random whitespace, valid and invalid numbers;
//! 4. arbitrary bytes.
//!
//! The parser must never panic on any input, comparable or not.
#![allow(clippy::unwrap_used)]

#[path = "support/comparable.rs"]
mod comparable;
#[path = "support/mutate.rs"]
mod mutate;
#[path = "support/binance_oracle.rs"]
mod oracle;

use comparable::comparable;
use mutate::{FIELDS, mutate, mutation};

use ob_core::InstrumentSpec;
use oracle::{fast, oracle as reference};
use proptest::prelude::*;

const FIXTURE: &str = include_str!("fixtures/binance_depth20.jsonl");
const TICKERS: &str = include_str!("fixtures/binance_bookticker.jsonl");

/// Recorded frame `i` of either fixture (depth first, then bookTicker).
fn recorded(i: usize) -> &'static [u8] {
    FIXTURE
        .lines()
        .chain(TICKERS.lines())
        .nth(i)
        .unwrap()
        .as_bytes()
}

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

fn check(msg: &[u8]) -> Result<(), TestCaseError> {
    let got = fast(msg, &spec(), "BTCUSDT"); // must not panic, whatever the input
    if comparable(msg) {
        let want = reference(msg, &spec(), "BTCUSDT");
        prop_assert_eq!(got, want, "input: {}", String::from_utf8_lossy(msg));
    }
    Ok(())
}

#[test]
fn recorded_frames_agree() {
    for frame in FIXTURE.lines().chain(TICKERS.lines()) {
        let want = reference(frame.as_bytes(), &spec(), "BTCUSDT");
        assert_ne!(want, oracle::OracleResult::Reject);
        assert_eq!(fast(frame.as_bytes(), &spec(), "BTCUSDT"), want);
    }
}

/// A generated message: some subset of the fields (sometimes duplicated), plus unknown
/// fields, in random order, with random whitespace.
fn generated() -> impl Strategy<Value = Vec<u8>> {
    let int = prop_oneof![
        (0_u64..2_000_000_000_000).prop_map(|n| n.to_string()),
        Just("01".to_string()),
        Just("-1".to_string()),
        Just("1.5".to_string()),
        Just("18446744073709551615".to_string()),
        Just("18446744073709551616".to_string()),
        Just("\"10\"".to_string()),
    ];
    let decimal = prop_oneof![
        (0_u32..1_000_000, 0_u32..10).prop_map(|(i, f)| format!("\"{i}.{f}0\"")), // on tick
        (0_u32..1_000_000, 0_u32..100).prop_map(|(i, f)| format!("\"{i}.{f:02}\"")), // maybe off
        (0_u32..10_000, 0_u32..1000).prop_map(|(i, f)| format!("\"{i}.{f:03}\"")),
        Just("\"-1.0\"".to_string()),
        Just("\"1e2\"".to_string()),
        Just("\"\"".to_string()),
        Just("1.5".to_string()),
    ];
    let level = (decimal.clone(), decimal.clone()).prop_map(|(p, q)| format!("[{p},{q}]"));
    let levels = prop::collection::vec(level, 0..5).prop_map(|v| format!("[{}]", v.join(",")));
    let string = prop_oneof![
        Just("\"depthUpdate\"".to_string()),
        Just("\"bookTicker\"".to_string()),
        Just("\"BTCUSDT\"".to_string()),
        Just("\"ETHUSDT\"".to_string()),
        Just("\"trade\"".to_string()),
    ];
    let junk = prop_oneof![
        Just("null".to_string()),
        Just("[1,{\"z\":[true,false]}]".to_string()),
        Just("{\"code\":2,\"msg\":\"bad\"}".to_string()),
        Just("-0.5e-7".to_string()),
    ];
    let field = prop_oneof![
        string.clone().prop_map(|v| format!("\"e\":{v}")),
        int.clone().prop_map(|v| format!("\"E\":{v}")),
        int.clone().prop_map(|v| format!("\"T\":{v}")),
        string.prop_map(|v| format!("\"s\":{v}")),
        int.clone().prop_map(|v| format!("\"U\":{v}")),
        int.clone().prop_map(|v| format!("\"u\":{v}")),
        int.clone().prop_map(|v| format!("\"pu\":{v}")),
        levels.clone().prop_map(|v| format!("\"b\":{v}")),
        levels.prop_map(|v| format!("\"a\":{v}")),
        // bookTicker forms: `b`/`a` as price strings, `B`/`A` quantities.
        decimal.clone().prop_map(|v| format!("\"b\":{v}")),
        decimal.clone().prop_map(|v| format!("\"a\":{v}")),
        decimal.clone().prop_map(|v| format!("\"B\":{v}")),
        decimal.clone().prop_map(|v| format!("\"A\":{v}")),
        int.prop_map(|v| format!("\"id\":{v}")),
        junk.clone().prop_map(|v| format!("\"result\":{v}")),
        junk.clone().prop_map(|v| format!("\"error\":{v}")),
        junk.prop_map(|v| format!("\"ps\":{v}")),
    ];
    let ws = prop::sample::select(vec!["", "", " ", "\n", " \t "]);
    let ticker_base: Vec<String> = [
        r#""e":"bookTicker""#,
        r#""u":12241724128962"#,
        r#""s":"BTCUSDT""#,
        r#""b":"59953.90""#,
        r#""B":"14.704""#,
        r#""a":"59954.00""#,
        r#""A":"0.839""#,
        r#""T":1795639643568"#,
        r#""E":1795639643569"#,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let base: Vec<String> = [
        r#""e":"depthUpdate""#,
        r#""E":1795610780582"#,
        r#""T":1795610780581"#,
        r#""s":"BTCUSDT""#,
        r#""U":12235913555065"#,
        r#""u":12235913562909"#,
        r#""pu":12235913554835"#,
        r#""b":[["58631.90","6.774"],["58631.80","0.002"]]"#,
        r#""a":[["58632.00","3.242"]]"#,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let unknown = prop::sample::select(FIELDS.to_vec())
        .prop_map(|f| f.trim_end().trim_end_matches(',').to_string());
    let complete = (
        prop_oneof![Just(base).prop_shuffle(), Just(ticker_base).prop_shuffle()],
        prop::collection::vec(unknown, 0..3),
        prop::option::weighted(0.3, (0_usize..9, field.clone())),
        ws.clone(),
    )
        .prop_map(|(mut fields, extra, replace, w)| {
            // Occasionally swap one known field for a random variant (maybe invalid,
            // maybe a different key: that makes it missing or duplicated).
            if let Some((i, f)) = replace {
                fields[i] = f;
            }
            fields.extend(extra);
            format!("{{{w}{}{w}}}", fields.join(&format!("{w},{w}"))).into_bytes()
        });
    let random = prop::collection::vec((field, ws), 0..16).prop_map(|fields| {
        let parts: Vec<String> = fields
            .into_iter()
            .map(|(f, w)| format!("{w}{f}{w}"))
            .collect();
        format!("{{{}}}", parts.join(",")).into_bytes()
    });
    prop_oneof![3 => complete, 1 => random]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 20_000, ..ProptestConfig::default() })]

    #[test]
    fn mutated_real_frames_agree(
        frame in 0_usize..120,
        muts in prop::collection::vec(mutation(), 1..4),
    ) {
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

/// Guards the quality of the tests above: a generator that only ever produces rejected
/// input would "agree" trivially. Each must produce plenty of accepted *and* rejected
/// messages.
#[test]
fn generators_exercise_both_accept_and_reject() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let mut rate = |strategy: &dyn Fn(&mut TestRunner) -> Vec<u8>| {
        let n = 2_000;
        let accepted = (0..n)
            .filter(|_| {
                reference(&strategy(&mut runner), &spec(), "BTCUSDT")
                    != oracle::OracleResult::Reject
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
    // Both message kinds must be accepted sometimes, or one path is untested.
    let tickers = (0..2_000)
        .filter(|_| {
            matches!(
                reference(
                    &generated().new_tree(&mut runner).unwrap().current(),
                    &spec(),
                    "BTCUSDT"
                ),
                oracle::OracleResult::Ticker { .. }
            )
        })
        .count();
    eprintln!("generated bookTickers accepted: {tickers} / 2000");
    assert!(
        tickers > 100,
        "the generator must produce valid bookTickers"
    );
    for r in [mutated, gen_rate] {
        assert!(
            (0.10..=0.90).contains(&r),
            "acceptance rate {r} too lopsided to test both paths"
        );
    }
}
