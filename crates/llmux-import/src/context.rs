use crate::{array, string, upstream::ProviderIdentity};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct Overrides {
    values: BTreeMap<(ProviderIdentity, String), u32>,
    used: BTreeSet<(ProviderIdentity, String)>,
    invalid: usize,
}

impl Overrides {
    pub(super) fn load(root: &Value, warnings: &mut Vec<String>) -> Self {
        let mut overrides = Self::default();
        if let Some(table) = root
            .get("modelContextOverrides")
            .filter(|table| !table.is_null())
        {
            if let Some(rows) = table.as_array() {
                for (index, row) in rows.iter().enumerate() {
                    let provider = string(row, "provider");
                    let model = if row.get("modelId").is_some() {
                        string(row, "modelId")
                    } else {
                        string(row, "model_id")
                    };
                    let limit = row
                        .get("realContext")
                        .or_else(|| row.get("real_context"))
                        .and_then(Value::as_u64)
                        .and_then(|limit| u32::try_from(limit).ok())
                        .filter(|limit| *limit > 0);
                    let key = (
                        crate::upstream::canonical_for_model(
                            provider,
                            model,
                            array(root, "providerNodes"),
                        ),
                        model.to_owned(),
                    );
                    if let Some(limit) = limit.filter(|_| {
                        !provider.is_empty()
                            && !model.is_empty()
                            && !overrides.values.contains_key(&key)
                    }) {
                        overrides.values.insert(key, limit);
                    } else {
                        overrides.invalid += 1;
                        warnings.push(format!("invalid context override row {index}: expected a unique provider/model and positive u32 token count"));
                    }
                }
            } else {
                overrides.invalid += 1;
                warnings.push("invalid context override table: expected an array".into());
            }
        }
        if let Some(table) = root
            .get("modelCapabilityOverrides")
            .filter(|table| !table.is_null())
        {
            if let Some(rows) = table.as_array().filter(|rows| !rows.is_empty()) {
                warnings.push(format!("{} capability overrides ignored: input-only, output-only and other capability limits are not total context windows", rows.len()));
            } else if !table.is_array() {
                warnings.push("invalid capability override table: expected an array".into());
            }
        }
        overrides
    }

    pub(super) fn limit(&mut self, provider: &ProviderIdentity, model: &str) -> Option<u32> {
        let key = (provider.clone(), model.to_owned());
        let limit = *self.values.get(&key)?;
        self.used.insert(key);
        Some(limit)
    }

    pub(super) fn reset_usage(&mut self) {
        self.used.clear();
    }

    pub(super) fn report(&self, warnings: &mut Vec<String>) {
        if !self.values.is_empty() || self.invalid > 0 {
            warnings.push(format!(
                "context overrides: {} applied, {} ignored (no imported deployment), {} invalid",
                self.used.len(),
                self.values.len() - self.used.len(),
                self.invalid
            ));
        }
    }
}
