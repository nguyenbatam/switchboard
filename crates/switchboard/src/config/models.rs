use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use super::pricing::Money;

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsFile {
    pub model: Vec<ModelSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelSpec {
    pub alias: String,
    pub provider: String,
    pub upstream_name: String,
    pub context: u32,
    /// What the tenant pays, USD per million tokens. Never zero on a billable model.
    pub price_in: f64,
    pub price_out: f64,
    /// What *we* pay upstream, USD per million tokens: the provider's list price for a
    /// hosted model, amortised GPU-hours for a local one. Defaults to zero so that a model
    /// added without a cost is visibly infinite-margin rather than silently loss-making.
    #[serde(default)]
    pub cost_in: f64,
    #[serde(default)]
    pub cost_out: f64,
    pub tokenizer: TokenizerSpec,
    #[serde(default)]
    pub fallback: Vec<String>,
    /// Fixed-point prices, converted once in `ModelRegistry::load`.
    #[serde(skip)]
    price_in_money: Money,
    #[serde(skip)]
    price_out_money: Money,
    #[serde(skip)]
    cost_in_money: Money,
    #[serde(skip)]
    cost_out_money: Money,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TokenizerSpec {
    Tiktoken { encoding: String },
    Hf { repo: String },
    Estimate,
}

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    models: HashMap<String, ModelSpec>,
}

impl ModelRegistry {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let content = fs::read_to_string(path.as_ref())
            .map_err(|e| format!("failed to read models config: {e}"))?;
        let file: ModelsFile =
            toml::from_str(&content).map_err(|e| format!("invalid models config: {e}"))?;

        let aliases: HashSet<String> = file.model.iter().map(|m| m.alias.clone()).collect();
        let mut models = HashMap::new();
        for mut spec in file.model {
            for fb in &spec.fallback {
                if !aliases.contains(fb) {
                    return Err(format!("fallback alias '{fb}' does not exist"));
                }
            }
            if spec.cost_in > spec.price_in || spec.cost_out > spec.price_out {
                return Err(format!(
                    "model '{}' sells below cost: price {}/{} vs cost {}/{}",
                    spec.alias, spec.price_in, spec.price_out, spec.cost_in, spec.cost_out
                ));
            }
            spec.price_in_money = Money::from_usd_per_million(spec.price_in);
            spec.price_out_money = Money::from_usd_per_million(spec.price_out);
            spec.cost_in_money = Money::from_usd_per_million(spec.cost_in);
            spec.cost_out_money = Money::from_usd_per_million(spec.cost_out);
            models.insert(spec.alias.clone(), spec);
        }
        Ok(Self { models })
    }

    #[cfg(test)]
    pub fn for_test(alias: &str, tokenizer: TokenizerSpec) -> Self {
        let spec = ModelSpec {
            alias: alias.into(),
            provider: "test".into(),
            upstream_name: alias.into(),
            context: 8192,
            price_in: 0.0,
            price_out: 0.0,
            cost_in: 0.0,
            cost_out: 0.0,
            tokenizer,
            fallback: Vec::new(),
            price_in_money: Money::default(),
            price_out_money: Money::default(),
            cost_in_money: Money::default(),
            cost_out_money: Money::default(),
        };
        Self {
            models: HashMap::from([(alias.to_string(), spec)]),
        }
    }

    #[cfg(test)]
    pub fn insert_for_test(&mut self, alias: &str, provider: &str, price_in: f64, price_out: f64) {
        // Half the sell price, so tests that assert on margin have a non-trivial one.
        let (cost_in, cost_out) = (price_in / 2.0, price_out / 2.0);
        self.models.insert(
            alias.to_string(),
            ModelSpec {
                alias: alias.into(),
                provider: provider.into(),
                upstream_name: alias.into(),
                context: 8192,
                price_in,
                price_out,
                cost_in,
                cost_out,
                tokenizer: TokenizerSpec::Estimate,
                fallback: Vec::new(),
                price_in_money: Money::from_usd_per_million(price_in),
                price_out_money: Money::from_usd_per_million(price_out),
                cost_in_money: Money::from_usd_per_million(cost_in),
                cost_out_money: Money::from_usd_per_million(cost_out),
            },
        );
    }

    pub fn get(&self, alias: &str) -> Option<&ModelSpec> {
        self.models.get(alias)
    }

    pub fn list(&self) -> impl Iterator<Item = &ModelSpec> {
        self.models.values()
    }

    pub fn cost_nano(&self, alias: &str, prompt: u32, completion: u32) -> i64 {
        let Some(spec) = self.get(alias) else {
            return 0;
        };
        Self::cost_nano_for_spec(spec, prompt, completion)
    }

    /// Revenue: what the tenant is billed.
    pub fn cost_nano_for_spec(spec: &ModelSpec, prompt: u32, completion: u32) -> i64 {
        spec.price_in_money.cost_nano(prompt) + spec.price_out_money.cost_nano(completion)
    }

    /// Cost of goods: what we pay the provider. Never leaves the process boundary — a
    /// tenant that can see our margin can negotiate against it.
    pub fn upstream_cost_nano_for_spec(spec: &ModelSpec, prompt: u32, completion: u32) -> i64 {
        spec.cost_in_money.cost_nano(prompt) + spec.cost_out_money.cost_nano(completion)
    }

    /// Alias used by usage recorder without holding registry.
    pub fn cost_nano_static(spec: &ModelSpec, prompt: u32, completion: u32) -> i64 {
        Self::cost_nano_for_spec(spec, prompt, completion)
    }
}

#[cfg(test)]
mod margin_tests {
    use super::*;
    use std::io::Write;

    fn spec(price: (f64, f64), cost: (f64, f64)) -> ModelSpec {
        ModelSpec {
            alias: "m".into(),
            provider: "p".into(),
            upstream_name: "m".into(),
            context: 1024,
            price_in: price.0,
            price_out: price.1,
            cost_in: cost.0,
            cost_out: cost.1,
            tokenizer: TokenizerSpec::Estimate,
            fallback: Vec::new(),
            price_in_money: Money::from_usd_per_million(price.0),
            price_out_money: Money::from_usd_per_million(price.1),
            cost_in_money: Money::from_usd_per_million(cost.0),
            cost_out_money: Money::from_usd_per_million(cost.1),
        }
    }

    #[test]
    fn revenue_and_cost_are_different_numbers() {
        // The local model in config/models.toml: sold at 0.05/0.10, costs 0.01/0.02.
        let s = spec((0.05, 0.10), (0.01, 0.02));
        let (prompt, completion) = (50, 200);

        let revenue = ModelRegistry::cost_nano_for_spec(&s, prompt, completion);
        let cogs = ModelRegistry::upstream_cost_nano_for_spec(&s, prompt, completion);

        assert_eq!(revenue, 22_500, "what the tenant pays");
        assert_eq!(cogs, 4_500, "what we pay for the GPU-seconds");
        assert_eq!(
            revenue - cogs,
            18_000,
            "the margin, and the reason to exist"
        );
    }

    #[test]
    fn reselling_a_hosted_model_carries_a_thin_margin() {
        // gpt-4o-mini: we pay OpenAI 0.15/0.60 and sell at 0.20/0.80.
        let s = spec((0.20, 0.80), (0.15, 0.60));
        let revenue = ModelRegistry::cost_nano_for_spec(&s, 50, 200);
        let cogs = ModelRegistry::upstream_cost_nano_for_spec(&s, 50, 200);
        assert_eq!(revenue, 170_000);
        assert_eq!(cogs, 127_500);
        // 25% margin. A caller who only ever uses this model should go direct, and
        // examples/bot-fleet's report says so.
        assert_eq!(revenue - cogs, 42_500);
    }

    #[test]
    fn a_model_with_no_declared_cost_reports_full_margin_not_a_loss() {
        // `cost_*` defaults to zero. That is wrong in an obvious direction — infinite
        // margin — rather than a plausible one. Nobody ships a pricing bug they can see.
        let s = spec((0.05, 0.10), (0.0, 0.0));
        assert_eq!(ModelRegistry::upstream_cost_nano_for_spec(&s, 50, 200), 0);
        assert!(ModelRegistry::cost_nano_for_spec(&s, 50, 200) > 0);
    }

    fn load_toml(body: &str) -> Result<ModelRegistry, String> {
        let mut f = tempfile();
        f.write_all(body.as_bytes()).unwrap();
        ModelRegistry::load(f.path())
    }

    fn tempfile() -> tempfile_shim::NamedTemp {
        tempfile_shim::NamedTemp::new()
    }

    #[test]
    fn a_model_that_sells_below_cost_refuses_to_load() {
        // The failure this prevents is not a crash. It is a gateway that serves happily
        // for a month and loses money on every request, which nobody notices until the
        // provider's invoice exceeds the customers'.
        let err = load_toml(
            r#"
            [[model]]
            alias = "loss-leader"
            provider = "openai"
            upstream_name = "gpt-4o-mini"
            context = 128000
            price_in = 0.10
            price_out = 0.20
            cost_in = 0.15
            cost_out = 0.60
            tokenizer = { kind = "estimate" }
            "#,
        )
        .expect_err("must refuse to load");
        assert!(err.contains("sells below cost"), "got: {err}");
    }

    #[test]
    fn a_model_priced_at_cost_still_loads() {
        // Zero margin is a business decision, not a typo. Only *below* cost is refused.
        load_toml(
            r#"
            [[model]]
            alias = "at-cost"
            provider = "ollama"
            upstream_name = "x"
            context = 1024
            price_in = 0.01
            price_out = 0.02
            cost_in = 0.01
            cost_out = 0.02
            tokenizer = { kind = "estimate" }
            "#,
        )
        .expect("zero margin is legal");
    }

    /// A two-line stand-in for the `tempfile` crate: this is the only place that needs one.
    mod tempfile_shim {
        use std::path::{Path, PathBuf};

        pub struct NamedTemp(PathBuf);

        impl NamedTemp {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "sb-models-{}-{}.toml",
                    std::process::id(),
                    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl std::io::Write for NamedTemp {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                std::fs::write(&self.0, buf)?;
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl Drop for NamedTemp {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    }
}
