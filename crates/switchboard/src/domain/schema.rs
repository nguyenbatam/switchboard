use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Input arguments a tool accepts — maps to OpenAI `tools[].function.parameters`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolParameters {
    #[serde(rename = "type", default)]
    pub kind: ToolParameterRootKind,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, FieldSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    #[serde(
        rename = "additionalProperties",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub extra_fields: Option<ExtraFieldsPolicy>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolParameterRootKind {
    #[default]
    Object,
}

/// One typed field inside tool parameters or structured output.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FieldSpec {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<FieldKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<BTreeMap<String, FieldSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    #[serde(
        rename = "additionalProperties",
        skip_serializing_if = "Option::is_none"
    )]
    pub extra_fields: Option<ExtraFieldsPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<FieldSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub allowed_values: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_items: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Object,
    String,
    Number,
    Integer,
    Boolean,
    Array,
    Null,
}

/// Whether callers may pass fields not listed in `properties`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ExtraFieldsPolicy {
    Allow(bool),
    FollowSpec(Box<FieldSpec>),
}

impl FieldSpec {
    pub fn string(description: impl Into<String>) -> Self {
        Self {
            kind: Some(FieldKind::String),
            description: Some(description.into()),
            ..Default::default()
        }
    }
}

impl ToolParameters {
    pub fn object(
        properties: BTreeMap<String, FieldSpec>,
        required: Vec<String>,
        allow_extra: bool,
    ) -> Self {
        Self {
            kind: ToolParameterRootKind::Object,
            properties,
            required: Some(required),
            extra_fields: Some(ExtraFieldsPolicy::Allow(allow_extra)),
        }
    }
}

/// Forces the model to reply with JSON matching a fixed shape.
/// Maps to OpenAI `response_format.json_schema`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StructuredOutput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub schema: FieldSpec,
    #[serde(default)]
    pub strict: bool,
}
