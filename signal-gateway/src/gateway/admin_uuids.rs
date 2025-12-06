//! Admin Signal UUIDs container with flexible deserialization.
//!
//! Supports two formats:
//! - Map: `{"uuid1": ["safety1", "safety2"], "uuid2": []}`
//! - Sequence: `["uuid1", "uuid2"]` (treated as UUIDs with no safety numbers)

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::fmt;

/// Container for admin Signal UUIDs mapped to their optional safety numbers.
///
/// Can be deserialized from either:
/// - A map of UUID -> safety numbers: `{"uuid1": ["12345..."], "uuid2": []}`
/// - A sequence of UUIDs (no safety numbers): `["uuid1", "uuid2"]`
#[derive(Clone, Debug, Default)]
pub struct AdminSignalUuids {
    map: HashMap<String, Vec<String>>,
}

impl AdminSignalUuids {
    /// Create an empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a UUID is a registered admin.
    pub fn contains(&self, uuid: &str) -> bool {
        self.map.contains_key(uuid)
    }

    /// Get all admin UUIDs.
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
}

impl<'de> Deserialize<'de> for AdminSignalUuids {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(AdminSignalUuidsVisitor)
    }
}

struct AdminSignalUuidsVisitor;

impl<'de> Visitor<'de> for AdminSignalUuidsVisitor {
    type Value = AdminSignalUuids;

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
        Ok(AdminSignalUuids { map })
    }

    fn visit_seq<S>(self, mut access: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        let mut map = HashMap::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(uuid) = access.next_element::<String>()? {
            map.insert(uuid, Vec::new());
        }
        Ok(AdminSignalUuids { map })
    }
}

impl FromIterator<String> for AdminSignalUuids {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Self {
            map: iter.into_iter().map(|uuid| (uuid, Vec::new())).collect(),
        }
    }
}

impl FromIterator<(String, Vec<String>)> for AdminSignalUuids {
    fn from_iter<I: IntoIterator<Item = (String, Vec<String>)>>(iter: I) -> Self {
        Self {
            map: iter.into_iter().collect(),
        }
    }
}

impl<'a> IntoIterator for &'a AdminSignalUuids {
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
        let uuids: AdminSignalUuids = serde_json::from_str(json).unwrap();

        assert_eq!(uuids.len(), 2);
        assert!(uuids.contains("uuid1"));
        assert!(uuids.contains("uuid2"));
        assert_eq!(uuids.get("uuid1").unwrap(), &vec!["safety1", "safety2"]);
        assert_eq!(uuids.get("uuid2").unwrap(), &Vec::<String>::new());
    }

    #[test]
    fn test_deserialize_seq() {
        let json = r#"["uuid1", "uuid2", "uuid3"]"#;
        let uuids: AdminSignalUuids = serde_json::from_str(json).unwrap();

        assert_eq!(uuids.len(), 3);
        assert!(uuids.contains("uuid1"));
        assert!(uuids.contains("uuid2"));
        assert!(uuids.contains("uuid3"));
        // All should have empty safety numbers
        assert!(uuids.get("uuid1").unwrap().is_empty());
        assert!(uuids.get("uuid2").unwrap().is_empty());
        assert!(uuids.get("uuid3").unwrap().is_empty());
    }

    #[test]
    fn test_empty_map() {
        let json = r#"{}"#;
        let uuids: AdminSignalUuids = serde_json::from_str(json).unwrap();
        assert!(uuids.is_empty());
    }

    #[test]
    fn test_empty_seq() {
        let json = r#"[]"#;
        let uuids: AdminSignalUuids = serde_json::from_str(json).unwrap();
        assert!(uuids.is_empty());
    }
}
