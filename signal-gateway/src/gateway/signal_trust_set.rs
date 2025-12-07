//! Signal trust set - a set of Signal UUIDs with optional safety numbers.
//!
//! Supports two deserialization formats:
//! - Map: `{"uuid1": ["safety1", "safety2"], "uuid2": []}` - UUIDs with safety numbers
//! - Sequence: `["uuid1", "uuid2"]` - UUIDs with no safety numbers (simpler)

use crate::signal_jsonrpc::{Envelope, Identity, RpcClient};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;
use tracing::{debug, info, warn};

/// A validated Signal UUID in the format `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Uuid(String);

impl FromStr for Uuid {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // UUID format: 8-4-4-4-12 hex chars (36 chars total with dashes)
        if s.len() != 36 {
            return Err(format!("UUID must be 36 characters, got {}", s.len()));
        }

        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 5 {
            return Err(format!(
                "UUID must have 5 dash-separated parts, got {}",
                parts.len()
            ));
        }

        let expected_lens = [8, 4, 4, 4, 12];
        for (i, (part, &expected)) in parts.iter().zip(&expected_lens).enumerate() {
            if part.len() != expected {
                return Err(format!(
                    "UUID part {} has wrong length: expected {}, got {}",
                    i + 1,
                    expected,
                    part.len()
                ));
            }
            if !part.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("UUID part {} contains non-hex characters", i + 1));
            }
        }

        Ok(Uuid(s.to_owned()))
    }
}

impl TryFrom<String> for Uuid {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl TryFrom<&str> for Uuid {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl Deref for Uuid {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for Uuid {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Uuid {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A validated Signal safety number (60 digits, optionally separated by whitespace).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct SafetyNumber(String);

impl FromStr for SafetyNumber {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Safety number format: 60 digits, optionally grouped with whitespace
        let mut digit_count = 0;
        for c in s.chars() {
            if c.is_ascii_digit() {
                digit_count += 1;
            } else if !c.is_whitespace() {
                return Err(format!(
                    "Safety number must contain only digits and whitespace, found '{c}'"
                ));
            }
        }

        if digit_count != 60 {
            return Err(format!(
                "Safety number must contain exactly 60 digits, got {digit_count}"
            ));
        }

        Ok(SafetyNumber(s.to_owned()))
    }
}

impl TryFrom<String> for SafetyNumber {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl TryFrom<&str> for SafetyNumber {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl Deref for SafetyNumber {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for SafetyNumber {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for SafetyNumber {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SafetyNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A set of Signal UUIDs with optional safety numbers for trust verification.
///
/// Can be deserialized from either:
/// - A map of UUID -> safety numbers: `{"uuid1": ["12345..."], "uuid2": []}`
/// - A sequence of UUIDs (no safety numbers): `["uuid1", "uuid2"]`
#[derive(Clone, Debug, Default)]
pub struct SignalTrustSet {
    map: HashMap<Uuid, Vec<SafetyNumber>>,
}

impl SignalTrustSet {
    /// Create an empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if the sender of an envelope is trusted.
    ///
    /// Currently checks if the source UUID is in the trust set.
    /// Will eventually also verify safety numbers.
    pub fn is_trusted(&self, envelope: &Envelope) -> bool {
        self.map.contains_key(envelope.source_uuid.as_str())
    }

    /// Get all UUIDs as an iterator of string slices.
    pub fn uuids(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(|u| u.as_ref())
    }

    /// Get the number of admin UUIDs.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate over UUID and safety number pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&Uuid, &Vec<SafetyNumber>)> {
        self.map.iter()
    }

    /// Get safety numbers for a specific UUID.
    pub fn get(&self, uuid: &str) -> Option<&Vec<SafetyNumber>> {
        self.map.get(uuid)
    }

    /// Update trust for all UUIDs with safety numbers configured.
    ///
    /// For each UUID with safety numbers:
    /// 1. Check current identities in signal-cli via listIdentities
    /// 2. If any trusted identity is NOT in our config, remove the contact entirely and re-add only configured ones
    /// 3. Otherwise, just trust any new safety numbers from config that aren't already trusted
    ///
    /// Returns an error if any trust operation fails.
    pub async fn update_trust(
        &self,
        signal_cli: &impl RpcClient,
        signal_account: &str,
    ) -> Result<(), String> {
        info!("Updating trust for {} configured UUIDs", self.map.len());

        for (uuid, safety_numbers) in &self.map {
            if safety_numbers.is_empty() {
                continue;
            }

            // Helper to check if a safety number string is in our configured list
            let is_configured = |sn: &str| safety_numbers.iter().any(|s| s.as_ref() == sn);

            // Get current identities from signal-cli
            let current_identities: Vec<Identity> = match signal_cli
                .list_identities(Some(signal_account.to_owned()), Some(uuid.to_string()))
                .await
            {
                Ok(value) => serde_json::from_value(value).unwrap_or_default(),
                Err(err) => {
                    debug!("Could not list identities for {uuid} (may not exist yet): {err}");
                    Vec::new()
                }
            };

            // Check if any trusted identity in signal-cli is NOT in our config
            let has_revoked_identity = current_identities
                .iter()
                .any(|id| id.trust_level.is_trusted() && !is_configured(&id.safety_number));

            if has_revoked_identity {
                // Log which identities are being revoked
                for id in &current_identities {
                    if id.trust_level.is_trusted() && !is_configured(&id.safety_number) {
                        warn!(
                            "Revoking trust for {uuid}: safety number {} is trusted in signal-cli but not in config",
                            id.safety_number
                        );
                    }
                }

                // Remove contact to clear all existing trust
                warn!("Resetting all trust for {uuid} due to revoked identity");
                signal_cli
                    .remove_contact(
                        Some(signal_account.to_owned()),
                        uuid.to_string(),
                        true,  // forget - delete identity keys and sessions
                        false, // hide
                    )
                    .await
                    .map_err(|err| format!("Failed to remove contact {uuid}: {err}"))?;

                // Re-add all configured safety numbers
                for safety_number in safety_numbers {
                    info!("Trusting safety number for {uuid}");
                    signal_cli
                        .trust(
                            Some(signal_account.to_owned()),
                            uuid.to_string(),
                            false,
                            Some(safety_number.to_string()),
                        )
                        .await
                        .map_err(|err| format!("Failed to trust {uuid}: {err}"))?;
                }

                // Verify the reset worked correctly
                let new_identities: Vec<Identity> = signal_cli
                    .list_identities(Some(signal_account.to_owned()), Some(uuid.to_string()))
                    .await
                    .map_err(|err| format!("Failed to verify trust reset for {uuid}: {err}"))
                    .and_then(|value| {
                        serde_json::from_value(value)
                            .map_err(|err| format!("Failed to parse identities for {uuid}: {err}"))
                    })?;

                let trusted_now: Vec<&str> = new_identities
                    .iter()
                    .filter(|id| id.trust_level.is_trusted())
                    .map(|id| id.safety_number.as_str())
                    .collect();

                // Check all configured safety numbers are now trusted
                for safety_number in safety_numbers {
                    if !trusted_now.contains(&safety_number.as_ref()) {
                        return Err(format!(
                            "Verification failed for {uuid}: safety number {} should be trusted but isn't",
                            safety_number
                        ));
                    }
                }

                // Check no unexpected safety numbers are trusted
                for sn in &trusted_now {
                    if !is_configured(sn) {
                        return Err(format!(
                            "Verification failed for {uuid}: safety number {} is trusted but not in config",
                            sn
                        ));
                    }
                }

                info!(
                    "Trust reset verified for {uuid}: {} safety numbers trusted",
                    trusted_now.len()
                );
            } else {
                // Just add any new safety numbers that aren't already trusted
                let already_trusted: Vec<&str> = current_identities
                    .iter()
                    .filter(|id| id.trust_level.is_trusted())
                    .map(|id| id.safety_number.as_str())
                    .collect();

                for safety_number in safety_numbers {
                    if !already_trusted.contains(&safety_number.as_ref()) {
                        info!("Trusting new safety number for {uuid}");
                        signal_cli
                            .trust(
                                Some(signal_account.to_owned()),
                                uuid.to_string(),
                                false,
                                Some(safety_number.to_string()),
                            )
                            .await
                            .map_err(|err| format!("Failed to trust {uuid}: {err}"))?;
                    }
                }
            }
        }

        Ok(())
    }
}

impl<'de> Deserialize<'de> for SignalTrustSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(SignalTrustSetVisitor)
    }
}

struct SignalTrustSetVisitor;

impl<'de> Visitor<'de> for SignalTrustSetVisitor {
    type Value = SignalTrustSet;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a map of UUIDs to safety numbers, or a sequence of UUIDs")
    }

    fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut map = HashMap::with_capacity(access.size_hint().unwrap_or(0));
        while let Some((key, value)) = access.next_entry::<Uuid, Vec<SafetyNumber>>()? {
            map.insert(key, value);
        }
        Ok(SignalTrustSet { map })
    }

    fn visit_seq<S>(self, mut access: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        let mut map = HashMap::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(uuid) = access.next_element::<Uuid>()? {
            map.insert(uuid, Vec::new());
        }
        Ok(SignalTrustSet { map })
    }
}

impl FromIterator<Uuid> for SignalTrustSet {
    fn from_iter<I: IntoIterator<Item = Uuid>>(iter: I) -> Self {
        Self {
            map: iter.into_iter().map(|uuid| (uuid, Vec::new())).collect(),
        }
    }
}

impl FromIterator<(Uuid, Vec<SafetyNumber>)> for SignalTrustSet {
    fn from_iter<I: IntoIterator<Item = (Uuid, Vec<SafetyNumber>)>>(iter: I) -> Self {
        Self {
            map: iter.into_iter().collect(),
        }
    }
}

impl<'a> IntoIterator for &'a SignalTrustSet {
    type Item = (&'a Uuid, &'a Vec<SafetyNumber>);
    type IntoIter = std::collections::hash_map::Iter<'a, Uuid, Vec<SafetyNumber>>;

    fn into_iter(self) -> Self::IntoIter {
        self.map.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID1: &str = "12345678-1234-1234-1234-123456789abc";
    const UUID2: &str = "abcdef12-abcd-abcd-abcd-abcdef123456";
    const UUID3: &str = "00000000-0000-0000-0000-000000000000";
    const SAFETY1: &str = "123456789012345678901234567890123456789012345678901234567890";
    const SAFETY2: &str = "098765432109876543210987654321098765432109876543210987654321";

    #[test]
    fn test_uuid_validation() {
        assert!(UUID1.parse::<Uuid>().is_ok());
        assert!("not-a-uuid".parse::<Uuid>().is_err());
        assert!("12345678-1234-1234-1234-12345678".parse::<Uuid>().is_err()); // too short
        assert!("12345678-1234-1234-1234-123456789abcdef".parse::<Uuid>().is_err()); // too long
        assert!("12345678-1234-1234-1234-123456789xyz".parse::<Uuid>().is_err()); // non-hex
    }

    #[test]
    fn test_safety_number_validation() {
        assert!(SAFETY1.parse::<SafetyNumber>().is_ok());
        // With whitespace (common format)
        assert!("12345 67890 12345 67890 12345 67890 12345 67890 12345 67890 12345 67890"
            .parse::<SafetyNumber>()
            .is_ok());
        assert!("12345".parse::<SafetyNumber>().is_err()); // too short
        assert!("12345678901234567890123456789012345678901234567890123456789x"
            .parse::<SafetyNumber>()
            .is_err()); // non-digit
    }

    #[test]
    fn test_deserialize_map() {
        let json = format!(
            r#"{{"{UUID1}": ["{SAFETY1}", "{SAFETY2}"], "{UUID2}": []}}"#
        );
        let trust_set: SignalTrustSet = serde_json::from_str(&json).unwrap();

        assert_eq!(trust_set.len(), 2);
        assert!(trust_set.get(UUID1).is_some());
        assert!(trust_set.get(UUID2).is_some());
        assert_eq!(trust_set.get(UUID1).unwrap().len(), 2);
        assert!(trust_set.get(UUID2).unwrap().is_empty());
    }

    #[test]
    fn test_deserialize_seq() {
        let json = format!(r#"["{UUID1}", "{UUID2}", "{UUID3}"]"#);
        let trust_set: SignalTrustSet = serde_json::from_str(&json).unwrap();

        assert_eq!(trust_set.len(), 3);
        assert!(trust_set.get(UUID1).is_some());
        assert!(trust_set.get(UUID2).is_some());
        assert!(trust_set.get(UUID3).is_some());
        // All should have empty safety numbers
        assert!(trust_set.get(UUID1).unwrap().is_empty());
        assert!(trust_set.get(UUID2).unwrap().is_empty());
        assert!(trust_set.get(UUID3).unwrap().is_empty());
    }

    #[test]
    fn test_empty_map() {
        let json = r#"{}"#;
        let uuids: SignalTrustSet = serde_json::from_str(json).unwrap();
        assert!(uuids.is_empty());
    }

    #[test]
    fn test_empty_seq() {
        let json = r#"[]"#;
        let uuids: SignalTrustSet = serde_json::from_str(json).unwrap();
        assert!(uuids.is_empty());
    }

    #[test]
    fn test_invalid_uuid_rejected() {
        let json = r#"["not-a-valid-uuid"]"#;
        assert!(serde_json::from_str::<SignalTrustSet>(json).is_err());
    }

    #[test]
    fn test_invalid_safety_number_rejected() {
        let json = format!(r#"{{"{UUID1}": ["invalid"]}}"#);
        assert!(serde_json::from_str::<SignalTrustSet>(&json).is_err());
    }
}
