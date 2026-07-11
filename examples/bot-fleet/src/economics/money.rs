//! Nanodollars, and nothing else.
//!
//! `docs/00`: *"Money is `bigint` nanodollars (1e-9 USD). Never `f64`. `gpt-4o-mini` input
//! is $0.00000015/token; in floating point that accumulates visible error over a month of
//! traffic, and 'visible error in the billing table' is not a sentence anyone wants to
//! write."*
//!
//! The gateway obeys that rule. A client that reconciles against the gateway's ledger has
//! to obey it too, or the reconciliation reports a drift that exists only in the client.
//! We already learned this once, in [`crate::contract::retry_budget`].

/// Nanodollars. 1 USD = 1_000_000_000.
pub type Nano = i64;

pub const NANO_PER_USD: i64 = 1_000_000_000;

/// Exact. No rounding, no `f64`, ever.
pub fn usd(nano: Nano) -> String {
    let sign = if nano < 0 { "-" } else { "" };
    let n = nano.unsigned_abs();
    format!("{sign}{}.{:09}", n / 1_000_000_000, n % 1_000_000_000)
}

/// Rendered for humans, to cents, still without floating point.
pub fn usd_short(nano: Nano) -> String {
    let sign = if nano < 0 { "-" } else { "" };
    let n = nano.unsigned_abs();
    // Round half-up at the cent, in integers.
    let cents = (n + 5_000_000) / 10_000_000;
    format!("{sign}{}.{:02}", cents / 100, cents % 100)
}

/// `numerator / denominator` as basis points (1bp = 0.01%), rounded toward zero.
///
/// Percentages are the one place a report is tempted back into `f64`. They do not need to
/// be: an integer ratio in basis points is exact, and divides cleanly for display.
pub fn basis_points(numerator: i64, denominator: i64) -> i64 {
    if denominator == 0 {
        return 0;
    }
    (numerator as i128 * 10_000 / denominator as i128) as i64
}

pub fn pct(bp: i64) -> String {
    let sign = if bp < 0 { "-" } else { "" };
    let b = bp.unsigned_abs();
    format!("{sign}{}.{:02}%", b / 100, b % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_renders_exactly() {
        assert_eq!(usd(150_000_000), "0.150000000");
        assert_eq!(usd(1_000_000_000), "1.000000000");
        assert_eq!(usd(1), "0.000000001");
        assert_eq!(usd(-500_000_000), "-0.500000000");
    }

    #[test]
    fn usd_short_rounds_half_up_at_the_cent() {
        assert_eq!(usd_short(0), "0.00");
        assert_eq!(usd_short(4_999_999), "0.00"); // just under half a cent
        assert_eq!(usd_short(5_000_000), "0.01"); // exactly half a cent
        assert_eq!(usd_short(1_234_000_000), "1.23");
        assert_eq!(usd_short(1_235_000_000), "1.24");
    }

    #[test]
    fn basis_points_are_exact_and_do_not_divide_by_zero() {
        assert_eq!(basis_points(1, 2), 5_000); // 50.00%
        assert_eq!(basis_points(1, 3), 3_333); // 33.33%, truncated
        assert_eq!(basis_points(0, 0), 0);
        assert_eq!(basis_points(5, 0), 0);
    }

    #[test]
    fn basis_points_survive_large_nanodollar_amounts() {
        // 900 USD saved of 1000 USD baseline. In i64 this multiplication would overflow
        // without the i128 widening: 900e9 * 10_000 > i64::MAX.
        let saved = 900 * NANO_PER_USD;
        let baseline = 1000 * NANO_PER_USD;
        assert_eq!(basis_points(saved, baseline), 9_000);
        assert_eq!(pct(9_000), "90.00%");
    }

    #[test]
    fn pct_renders_hundredths() {
        assert_eq!(pct(10_000), "100.00%");
        assert_eq!(pct(3_333), "33.33%");
        assert_eq!(pct(-250), "-2.50%");
    }
}
