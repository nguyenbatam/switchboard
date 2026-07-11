use crate::config::AppConfig;
use tracing_subscriber::{EnvFilter, fmt};

pub fn init_tracing(config: &AppConfig) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if config.telemetry.log_format == "json" {
        fmt().json().with_env_filter(filter).init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}
