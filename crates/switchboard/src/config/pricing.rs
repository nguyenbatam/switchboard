/// Fixed-point price. `f64` crosses the boundary exactly once, at config load;
/// every arithmetic step afterwards is integral.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Money {
    micros_per_million: i64,
}

impl Money {
    pub fn from_usd_per_million(usd: f64) -> Self {
        Self {
            micros_per_million: (usd * 1_000_000.0).round() as i64,
        }
    }

    pub fn cost_nano(self, tokens: u32) -> i64 {
        let nano_per_usd = 1_000_000_000i128;
        let tokens = tokens as i128;
        let micros = self.micros_per_million as i128;
        ((tokens * micros * nano_per_usd) / (1_000_000 * 1_000_000)) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt4o_mini_pricing() {
        let price_in = Money::from_usd_per_million(0.15);
        let price_out = Money::from_usd_per_million(0.60);
        let cost = price_in.cost_nano(1000) + price_out.cost_nano(500);
        assert!(cost > 0);
    }
}
