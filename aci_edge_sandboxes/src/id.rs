use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

/// Opaque identifier of a provisioned sandbox.
///
/// A sandbox ID has the form `aci-edge-sandboxes:<token>`, where the token is 32 lowercase
/// hexadecimal digits drawn from the operating system's cryptographically secure random number
/// generator. The prefix routes non-provision calls to ACI Edge Sandboxes; the token is opaque to
/// callers.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SandboxId(String);

impl SandboxId {
    /// Prefix shared by every ACI Edge Sandboxes ID.
    pub const PREFIX: &'static str = "aci-edge-sandboxes";

    const TOKEN_BYTES: usize = 16;
    const TOKEN_LEN: usize = Self::TOKEN_BYTES * 2;

    /// Mints a fresh sandbox ID.
    ///
    /// Backends call this when provisioning a sandbox.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; Self::TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|error| {
            Error::backend_error("failed to generate a sandbox ID").with_source(error)
        })?;
        Ok(Self(format!("{}:{}", Self::PREFIX, hex(&bytes))))
    }

    /// Parses a sandbox ID, returning [`ErrorCode::MalformedId`](crate::ErrorCode::MalformedId)
    /// when the value is not a structurally valid ACI Edge Sandboxes ID.
    pub fn parse(value: &str) -> Result<Self> {
        if value.is_empty() {
            return Err(Error::malformed_id("sandbox ID is empty"));
        }
        if value.contains('\0') {
            return Err(Error::malformed_id("sandbox ID contains a NUL character"));
        }
        let Some((prefix, token)) = value.split_once(':') else {
            return Err(Error::malformed_id(format!(
                "sandbox ID {value:?} has no prefix"
            )));
        };
        if prefix != Self::PREFIX {
            return Err(Error::malformed_id(format!(
                "sandbox ID prefix {prefix:?} is not {:?}",
                Self::PREFIX
            )));
        }
        if token.len() != Self::TOKEN_LEN
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::malformed_id(format!(
                "sandbox ID token must be {} lowercase hexadecimal digits",
                Self::TOKEN_LEN
            )));
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the full ID, including its prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the opaque token that follows the prefix.
    pub fn token(&self) -> &str {
        &self.0[Self::PREFIX.len() + 1..]
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

impl fmt::Debug for SandboxId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, formatter)
    }
}

impl fmt::Display for SandboxId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for SandboxId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for SandboxId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for SandboxId {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl TryFrom<String> for SandboxId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<SandboxId> for String {
    fn from(id: SandboxId) -> Self {
        id.0
    }
}

impl Serialize for SandboxId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SandboxId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(|error| serde::de::Error::custom(error.message()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    #[test]
    fn generated_ids_round_trip() {
        let id = SandboxId::generate().unwrap();
        assert!(id.as_str().starts_with("aci-edge-sandboxes:"));
        assert_eq!(id.token().len(), 32);
        assert_eq!(SandboxId::parse(id.as_str()).unwrap(), id);
        assert_ne!(SandboxId::generate().unwrap(), id);
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for value in [
            "",
            "aci-edge-sandboxes",
            "aci-edge-sandboxes:",
            "iso:0123456789abcdef0123456789abcdef",
            "aci-edge-sandboxes:0123456789ABCDEF0123456789ABCDEF",
            "aci-edge-sandboxes:0123456789abcdef0123456789abcde",
            "aci-edge-sandboxes:0123456789abcdef0123456789abcdef0",
            "aci-edge-sandboxes:../../../../../../../../etc/passwd",
            "aci-edge-sandboxes:0123456789abcdef\u{0}123456789abcdef",
        ] {
            let error = SandboxId::parse(value).unwrap_err();
            assert_eq!(error.code(), ErrorCode::MalformedId, "{value:?}");
        }
    }

    #[test]
    fn serde_uses_the_plain_string() {
        let id = SandboxId::parse("aci-edge-sandboxes:0123456789abcdef0123456789abcdef").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(
            json,
            r#""aci-edge-sandboxes:0123456789abcdef0123456789abcdef""#
        );
        assert_eq!(serde_json::from_str::<SandboxId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<SandboxId>(r#""aci-edge-sandboxes:zz""#).is_err());
    }
}
