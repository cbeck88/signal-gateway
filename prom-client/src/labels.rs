//! Helpers for extracting common labels from metrics before presenting them
use std::{
    collections::BTreeMap,
    fmt::{Debug, Display},
};

/// A set of labels extracted by ExtractLabels
pub type Labels = BTreeMap<String, String>;

/// Common labels extracted from a very generic sequence of metric labels (key value pairs)
pub struct ExtractLabels {
    pub name: String,
    pub common_labels: Labels,
    pub specific_labels: Vec<Labels>,
}

impl ExtractLabels {
    pub fn new<'a, I, KV, K, V>(src: I, skip_labels: &[String]) -> Self
    where
        I: Iterator<Item = &'a KV>,
        KV: Clone + Debug + 'a,
        K: Display + 'a,
        V: Display + 'a,
        &'a KV: IntoIterator<Item = (&'a K, &'a V)>,
    {
        // Common labels needs to be String -> Option<String>, because when we find a conflict,
        // we need to poison that key (by putting None)
        let mut common_labels = BTreeMap::<String, Option<String>>::default();
        let mut specific_labels: Vec<Labels> = src
            .map(|kv| {
                let labels: Labels = kv
                    .into_iter()
                    .map(|(k, v)| {
                        let k = k.to_string();
                        let v = v.to_string();
                        if let Some(existing_val) = common_labels.get_mut(&k) {
                            // If the existing value is None, we already eliminated this label,
                            // so don't add it back.
                            if let Some(common_val) = existing_val
                                && common_val != &v
                            {
                                *existing_val = None;
                            }
                        } else {
                            common_labels.insert(k.clone(), Some(v.clone()));
                        }
                        (k, v)
                    })
                    .collect();
                labels
            })
            .collect();

        let mut common_labels = common_labels
            .into_iter()
            .filter_map(|(k, maybe_v)| maybe_v.map(|v| (k, v)))
            .collect::<Labels>();

        for sl in skip_labels {
            common_labels.remove(sl);
        }
        for sp in &mut specific_labels {
            for sl in skip_labels {
                sp.remove(sl);
            }
            for cl in common_labels.keys() {
                sp.remove(cl);
            }
        }
        let name = common_labels.remove("__name__").unwrap_or_default();

        ExtractLabels {
            name,
            common_labels,
            specific_labels,
        }
    }
}
