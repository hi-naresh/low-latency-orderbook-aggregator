//! "Is `n` a multiple of `d`?" without a division instruction.
//!
//! The tick and lot checks run on every price and quantity we parse. `n % d` with a `d`
//! only known at run time compiles to a 64-bit `idiv`, which takes tens of cycles on
//! x86-64 (it measured as ~10 ns per parsed price on the i7-8569U). When `d` is a
//! compile-time constant the compiler avoids `idiv` with the same trick used here; our
//! tick sizes come from config at run time, so we precompute the constants once.
//!
//! The method (Granlund & Montgomery 1994; Hacker's Delight §10-17; Lemire et al. 2019):
//! write `d = odd · 2^k`. For an odd number there is a 64-bit "inverse" `inv` with
//! `odd · inv ≡ 1 (mod 2^64)`. Then `n` is a multiple of `d` exactly when
//! `rotate_right(n · inv, k) <= u64::MAX / d`, using wrapping multiplication.
//! That is one multiply, one rotate and one compare: a few cycles.

/// A precomputed divisibility test for a fixed positive divisor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Divisor {
    inv: u64,
    shift: u32,
    limit: u64,
}

impl Divisor {
    /// Precomputes the test for `d`. `d` must be non-zero (callers check this first).
    pub(crate) const fn new(d: u64) -> Divisor {
        assert!(d != 0, "divisor must be non-zero");
        let shift = d.trailing_zeros();
        let odd = d >> shift;
        // Newton's iteration for the inverse modulo 2^64. `odd` is its own inverse modulo
        // 8 (3 correct bits), and each step doubles the correct bits: 3→6→12→24→48→96.
        let mut inv = odd;
        let mut i = 0;
        while i < 5 {
            inv = inv.wrapping_mul(2_u64.wrapping_sub(odd.wrapping_mul(inv)));
            i += 1;
        }
        Divisor {
            inv,
            shift,
            limit: u64::MAX / d,
        }
    }

    /// `n / d` for an `n` that **is** a multiple of the divisor (checked with
    /// [`Divisor::divides`] first): exact division by a shift and a multiply, no `idiv`.
    /// For other `n` the result is meaningless (but never a panic).
    #[inline]
    pub(crate) const fn exact_quotient(&self, n: i64) -> i64 {
        let q = (n.unsigned_abs() >> self.shift)
            .wrapping_mul(self.inv)
            .cast_signed();
        if n < 0 { q.wrapping_neg() } else { q }
    }

    /// `true` if `n` is a multiple of the divisor (sign doesn't matter).
    #[inline]
    pub(crate) const fn divides(&self, n: i64) -> bool {
        n.unsigned_abs()
            .wrapping_mul(self.inv)
            .rotate_right(self.shift)
            <= self.limit
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_wrap)]
mod tests {
    use super::*;

    #[test]
    fn inverse_is_correct() {
        for d in [
            1_u64,
            3,
            5,
            7,
            9,
            25,
            125,
            999_999_999_999_999_999,
            u64::MAX,
        ] {
            let div = Divisor::new(d);
            assert_eq!(d.wrapping_mul(div.inv), 1, "d = {d}");
        }
    }

    #[test]
    fn exact_quotient_matches_division_for_multiples() {
        for d in [1_u64, 2, 3, 5, 10, 25, 64, 125, 1000, 999_983] {
            let div = Divisor::new(d);
            let d = d.cast_signed();
            for k in [-1_000_000_i64, -7, -1, 0, 1, 2, 7, 600_000, 1_000_000_007] {
                let n = k * d;
                assert_eq!(div.exact_quotient(n), k, "{n} / {d}");
            }
        }
    }

    #[test]
    fn matches_the_remainder_operator() {
        // Miri interprets ~100× slower than native: same coverage of shapes, fewer values.
        let max = if cfg!(miri) { 20 } else { 300 };
        let mut divisors: Vec<u64> = (1..=max).collect();
        divisors.extend([1_000, 1024, 5_000, 10_u64.pow(18), 3 << 40, i64::MAX as u64]);
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for &d in &divisors {
            let div = Divisor::new(d);
            let check = |n: i64| {
                assert_eq!(
                    div.divides(n),
                    n.unsigned_abs().is_multiple_of(d),
                    "n = {n}, d = {d}"
                );
            };
            let span: i64 = if cfg!(miri) { 50 } else { 2_000 };
            for n in -span..=span {
                check(n);
            }
            for edge in [i64::MAX, i64::MIN, i64::MAX - 1, i64::MIN + 1] {
                check(edge);
            }
            for _ in 0..(if cfg!(miri) { 20 } else { 2_000 }) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                check(x as i64);
                if let Some(m) = (x % 1_000).checked_mul(d) {
                    check(m as i64); // exact multiples
                    check(m.wrapping_add(1) as i64); // just past a multiple
                }
            }
        }
    }
}
