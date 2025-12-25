//! Contextual anchor used by agents to robustly locate a token in a buffer.
//!
//! Agents provide a small multi-word context that contains the token to locate,
//! plus an optional 1-based index to disambiguate repeated occurrences of the
//! token within that context. The resolver (server-side) is responsible for
//! locating the single matching occurrence in the in-memory buffer and returning
//! a precise Anchor/byte offset range pointing to the center of that token.
//!
//! This type is intentionally small and serde-friendly so it can be used across
//! ACP/tool boundaries.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A lightweight contextual anchor used by agents to identify a syntax position
/// robustly across edits.
///
/// - `path` is a project-relative path identifying the file/buffer.
/// - `context_str` is a short multi-word snippet that MUST include `token`.
/// - `token` is the exact token to locate within `context_str`.
/// - `index` is an optional 1-based selection index used when `token` appears
///   multiple times inside `context_str`. If omitted and multiple occurrences
///   exist, the resolver will treat that as an error and return feedback to the
///   agent (per your design requirements).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ContextualAnchor {
    /// Project-relative path (same format as other agent tools)
    pub path: String,

    /// A short multi-word context snippet that contains `token`.
    pub context_str: String,

    /// The specific token to locate inside the context snippet. Must appear in `context_str`.
    pub token: String,

    /// Optional 1-based index to disambiguate repeated occurrences of `token` in `context_str`.
    /// If provided it must be >= 1 and <= occurrences count. If omitted and multiple
    /// occurrences exist, the resolver will return an error.
    #[serde(default)]
    pub index: Option<usize>,
}

impl ContextualAnchor {
    /// Lightweight validation that can be used before sending to the resolver.
    /// Ensures `context_str` contains `token` and index (if provided) is >= 1.
    /// Detailed validation (ensuring a single occurrence when index is None)
    /// must be done by the resolver which has access to the buffer.
    pub fn validate_basic(&self) -> Result<(), ValidationError> {
        if self.context_str.is_empty() {
            return Err(ValidationError::MissingContext);
        }
        if self.token.is_empty() {
            return Err(ValidationError::MissingToken);
        }
        if !self.context_str.contains(&self.token) {
            return Err(ValidationError::ContextDoesNotContainToken {
                context: self.context_str.clone(),
                token: self.token.clone(),
            });
        }
        if let Some(ix) = self.index {
            if ix == 0 {
                return Err(ValidationError::InvalidIndex(ix));
            }
        }
        Ok(())
    }

    /// Count occurrences of `token` within `context_str`.
    /// Returns the byte offsets (relative to context_str start) of each match.
    /// This is a helper useful for quick pre-checks; the authoritative resolver
    /// should operate on the full buffer snapshot.
    pub fn token_occurrences_in_context(&self) -> Vec<usize> {
        let hay = self.context_str.as_str();
        let needle = self.token.as_str();
        let mut res = Vec::new();
        if needle.is_empty() {
            return res;
        }
        let mut start = 0usize;
        while let Some(pos) = hay[start..].find(needle) {
            res.push(start + pos);
            start = start + pos + needle.len();
        }
        res
    }
}

/// Errors returned by `ContextualAnchor` validation helpers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    MissingContext,
    MissingToken,
    ContextDoesNotContainToken { context: String, token: String },
    InvalidIndex(usize),
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::MissingContext => write!(f, "context_str is empty"),
            ValidationError::MissingToken => write!(f, "token is empty"),
            ValidationError::ContextDoesNotContainToken { context: _, token } => {
                write!(f, "context_str does not contain token `{}`", token)
            }
            ValidationError::InvalidIndex(ix) => {
                write!(f, "index must be 1-based and >= 1 (got {})", ix)
            }
        }
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_validation_passes() {
        let a = ContextualAnchor {
            path: "src/lib.rs".into(),
            context_str: "fn example(foo: i32) -> i32 { foo + 1 }".into(),
            token: "foo".into(),
            index: Some(1),
        };
        assert!(a.validate_basic().is_ok());
        let occ = a.token_occurrences_in_context();
        assert_eq!(occ.len(), 2);
    }

    #[test]
    fn missing_token_fails() {
        let a = ContextualAnchor {
            path: "src/lib.rs".into(),
            context_str: "something".into(),
            token: "".into(),
            index: None,
        };
        assert_eq!(
            a.validate_basic().unwrap_err(),
            ValidationError::MissingToken
        );
    }

    #[test]
    fn context_without_token_fails() {
        let a = ContextualAnchor {
            path: "src/lib.rs".into(),
            context_str: "some other text".into(),
            token: "needle".into(),
            index: None,
        };
        match a.validate_basic() {
            Err(ValidationError::ContextDoesNotContainToken { token, .. }) => {
                assert_eq!(token, "needle")
            }
            other => panic!("unexpected validation result: {:?}", other),
        }
    }

    #[test]
    fn invalid_index_fails() {
        let a = ContextualAnchor {
            path: "src/lib.rs".into(),
            context_str: "token token".into(),
            token: "token".into(),
            index: Some(0),
        };
        assert_eq!(
            a.validate_basic().unwrap_err(),
            ValidationError::InvalidIndex(0)
        );
    }
}
