use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub status: String,
}

impl StatusResponse {
    pub fn ok() -> Self {
        Self {
            status: "ok".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadyResponse {
    pub status: String,
    pub postgres: bool,
    pub redis: bool,
    pub providers: bool,
}

impl ReadyResponse {
    pub fn ready(postgres: bool, redis: bool, providers: bool) -> Self {
        let ready = postgres && redis && providers;
        Self {
            status: if ready { "ready" } else { "not_ready" }.into(),
            postgres,
            redis,
            providers,
        }
    }
}
