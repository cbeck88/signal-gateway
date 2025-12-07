//! Signal trust set - a set of Signal UUIDs with optional safety numbers.
//!
//! Supports two deserialization formats:
//! - Map: `{"uuid1": ["safety1", "safety2"], "uuid2": []}` - UUIDs with safety numbers
//! - Sequence: `["uuid1", "uuid2"]` - UUIDs with no safety numbers (simpler)

use crate::signal_jsonrpc::{Envelope, Identity, RpcClient};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::fmt;
use tracing::{debug, info, warn};

/// A set of Signal UUIDs with optional safety numbers for trust verification.
///
/// Can be deserialized from either:
/// - A map of UUID -> safety numbers: `{"uuid1": ["12345..."], "uuid2": []}`
/// - A sequence of UUIDs (no safety numbers): `["uuid1", "uuid2"]`
#[derive(Clone, Debug, Default)]
pub struct SignalTrustSet {
    map: HashMap<String, Vec<String>>,
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
        self.map.contains_key(&envelope.source_uuid)
    }

    /// Get all UUIDs as an iterator.
    pub fn uuids(&self) -> impl Iterator<Item = &String> {
        self.map.keys()
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
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Vec<String>)> {
        self.map.iter()
    }

    /// Get safety numbers for a specific UUID.
    pub fn get(&self, uuid: &str) -> Option<&Vec<String>> {
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

            // Get current identities from signal-cli
            let current_identities: Vec<Identity> = match signal_cli
                .list_identities(Some(signal_account.to_owned()), Some(uuid.clone()))
                .await
            {
                Ok(value) => serde_json::from_value(value).unwrap_or_default(),
                Err(err) => {
                    debug!("Could not list identities for {uuid} (may not exist yet): {err}");
                    Vec::new()
                }
            };

            // Check if any trusted identity in signal-cli is NOT in our config
            let has_revoked_identity = current_identities.iter().any(|id| {
                id.trust_level.is_trusted() && !safety_numbers.contains(&id.safety_number)
            });

            if has_revoked_identity {
                // Log which identities are being revoked
                for id in &current_identities {
                    if id.trust_level.is_trusted() && !safety_numbers.contains(&id.safety_number) {
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
                        uuid.clone(),
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
                            uuid.clone(),
                            false,
                            Some(safety_number.clone()),
                        )
                        .await
                        .map_err(|err| format!("Failed to trust {uuid}: {err}"))?;
                }

                // Verify the reset worked correctly
                let new_identities: Vec<Identity> = signal_cli
                    .list_identities(Some(signal_account.to_owned()), Some(uuid.clone()))
                    .await
                    .map_err(|err| format!("Failed to verify trust reset for {uuid}: {err}"))
                    .and_then(|value| {
                        serde_json::from_value(value)
                            .map_err(|err| format!("Failed to parse identities for {uuid}: {err}"))
                    })?;

                let trusted_now: Vec<_> = new_identities
                    .iter()
                    .filter(|id| id.trust_level.is_trusted())
                    .map(|id| &id.safety_number)
                    .collect();

                // Check all configured safety numbers are now trusted
                for safety_number in safety_numbers {
                    if !trusted_now.contains(&safety_number) {
                        return Err(format!(
                            "Verification failed for {uuid}: safety number {} should be trusted but isn't",
                            safety_number
                        ));
                    }
                }

                // Check no unexpected safety numbers are trusted
                for sn in &trusted_now {
                    if !safety_numbers.contains(sn) {
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
                let already_trusted: Vec<_> = current_identities
                    .iter()
                    .filter(|id| id.trust_level.is_trusted())
                    .map(|id| &id.safety_number)
                    .collect();

                for safety_number in safety_numbers {
                    if !already_trusted.contains(&safety_number) {
                        info!("Trusting new safety number for {uuid}");
                        signal_cli
                            .trust(
                                Some(signal_account.to_owned()),
                                uuid.clone(),
                                false,
                                Some(safety_number.clone()),
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
        while let Some((key, value)) = access.next_entry::<String, Vec<String>>()? {
            map.insert(key, value);
        }
        Ok(SignalTrustSet { map })
    }

    fn visit_seq<S>(self, mut access: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        let mut map = HashMap::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(uuid) = access.next_element::<String>()? {
            map.insert(uuid, Vec::new());
        }
        Ok(SignalTrustSet { map })
    }
}

impl FromIterator<String> for SignalTrustSet {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Self {
            map: iter.into_iter().map(|uuid| (uuid, Vec::new())).collect(),
        }
    }
}

impl FromIterator<(String, Vec<String>)> for SignalTrustSet {
    fn from_iter<I: IntoIterator<Item = (String, Vec<String>)>>(iter: I) -> Self {
        Self {
            map: iter.into_iter().collect(),
        }
    }
}

impl<'a> IntoIterator for &'a SignalTrustSet {
    type Item = (&'a String, &'a Vec<String>);
    type IntoIter = std::collections::hash_map::Iter<'a, String, Vec<String>>;

    fn into_iter(self) -> Self::IntoIter {
        self.map.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_map() {
        let json = r#"{"uuid1": ["safety1", "safety2"], "uuid2": []}"#;
        let trust_set: SignalTrustSet = serde_json::from_str(json).unwrap();

        assert_eq!(trust_set.len(), 2);
        assert!(trust_set.get("uuid1").is_some());
        assert!(trust_set.get("uuid2").is_some());
        assert_eq!(trust_set.get("uuid1").unwrap(), &vec!["safety1", "safety2"]);
        assert_eq!(trust_set.get("uuid2").unwrap(), &Vec::<String>::new());
    }

    #[test]
    fn test_deserialize_seq() {
        let json = r#"["uuid1", "uuid2", "uuid3"]"#;
        let trust_set: SignalTrustSet = serde_json::from_str(json).unwrap();

        assert_eq!(trust_set.len(), 3);
        assert!(trust_set.get("uuid1").is_some());
        assert!(trust_set.get("uuid2").is_some());
        assert!(trust_set.get("uuid3").is_some());
        // All should have empty safety numbers
        assert!(trust_set.get("uuid1").unwrap().is_empty());
        assert!(trust_set.get("uuid2").unwrap().is_empty());
        assert!(trust_set.get("uuid3").unwrap().is_empty());
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
}
