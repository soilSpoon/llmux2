//! Nous Portal subscription models (`nous/<portal id>`), free ones first.
//!
//! Each catalog entry becomes its own model with a single deployment: a
//! different vendor is never a fallback leg of another model's combo.

use crate::{policy::zero, string, unique};
use llmux_types::{NousState, NOUS_INFERENCE_URL};
use serde_json::{json, Value};
use std::{collections::HashSet, fs, path::Path};

pub(super) struct Nous {
    state_file: String,
    ids: Vec<String>,
}

fn free(row: &Value) -> bool {
    string(row, "id").ends_with(":free")
        || (zero(&row["pricing"]["prompt"]) && zero(&row["pricing"]["completion"]))
}

impl Nous {
    pub fn load(state: &Path, models: &Path) -> Result<Self, String> {
        let state = std::path::absolute(state).map_err(|_| "could not resolve --nous-state")?;
        if NousState::load(&state).is_err() {
            return Err(
                "--nous-state is missing or unreadable; run `llmux-login nous` first".into(),
            );
        }
        let state_file = state
            .to_str()
            .filter(|path| !path.contains(['\n', '\r']))
            .ok_or("--nous-state path must be UTF-8")?
            .to_owned();
        let listing = fs::read_to_string(models).map_err(|_| "could not read --nous-models")?;
        let listing: Value =
            serde_json::from_str(&listing).map_err(|_| "invalid --nous-models JSON")?;
        let rows = listing
            .as_array()
            .or_else(|| listing["data"].as_array())
            .ok_or("--nous-models must be an array or a /v1/models object with data")?;
        let mut seen = HashSet::new();
        let (mut ranked, paid): (Vec<_>, Vec<_>) = rows
            .iter()
            .filter(|row| !string(row, "id").is_empty() && seen.insert(string(row, "id")))
            .partition(|row| free(row));
        ranked.extend(paid);
        if ranked.is_empty() {
            return Err("--nous-models has no models; save the output of GET /v1/models".into());
        }
        Ok(Self {
            state_file,
            ids: ranked
                .iter()
                .map(|row| string(row, "id").to_owned())
                .collect(),
        })
    }

    pub fn credential(&self, id: &str) -> Value {
        json!({"id":id,"kind":"nous_portal","provider":"openai_compat","state_file":self.state_file})
    }

    pub fn models(
        &self,
        credential: &str,
        taken: &HashSet<String>,
        deployment_ids: &mut HashSet<String>,
        warnings: &mut Vec<String>,
    ) -> Vec<Value> {
        let mut models = Vec::new();
        for id in &self.ids {
            let name = format!("nous/{id}");
            if taken.contains(&name) {
                warnings.push(format!("nous {name}: collision; existing model wins"));
                continue;
            }
            let deployment = unique(&format!("nous-{id}"), deployment_ids);
            models.push(json!({"name":name,"aliases":[],"deployments":[{
                "id":deployment,"credential":credential,"base_url":NOUS_INFERENCE_URL,
                "upstream_model":id,"priority":0,"billing":"free"}]}));
        }
        models
    }
}
