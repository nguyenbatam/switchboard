use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Parsed arguments for a single tool call. Shape varies per tool at runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParsedToolArguments {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<ParsedToolArguments>),
    Object(BTreeMap<String, ParsedToolArguments>),
}

impl Default for ParsedToolArguments {
    fn default() -> Self {
        Self::Object(BTreeMap::new())
    }
}

impl ParsedToolArguments {
    pub fn parse_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn empty_object() -> Self {
        Self::Object(BTreeMap::new())
    }

    pub fn typed<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_value(serde_json::to_value(self).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    }
}

impl FromStr for ParsedToolArguments {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_json(s)
    }
}

impl fmt::Display for ParsedToolArguments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json_string())
    }
}
