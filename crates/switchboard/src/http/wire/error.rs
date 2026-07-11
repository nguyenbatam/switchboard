use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ErrorEnvelope<'a> {
    pub error: ErrorDetail<'a>,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail<'a> {
    pub message: &'a str,
    pub r#type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct ErrorEnvelopeOwned {
    pub error: ErrorDetailOwned,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetailOwned {
    pub message: String,
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}
