use serde_json::{json, Map, Value};

fn camel_case(column: &str) -> String {
    let mut parts = column.split('_');
    let mut name = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            name.extend(first.to_uppercase());
            name.extend(chars);
        }
    }
    name
}

fn json_object(value: &Value, table: &str, column: &str) -> Result<Value, String> {
    if value.is_null() {
        return Ok(json!({}));
    }
    let parsed = match value.as_str() {
        Some(text) => serde_json::from_str(text)
            .map_err(|_| format!("{table}.{column} must contain a JSON object"))?,
        None => value.clone(),
    };
    if !parsed.is_object() {
        return Err(format!("{table}.{column} must contain a JSON object"));
    }
    Ok(parsed)
}

/// api_keys columns that SQLite stores as JSON-encoded text. Other dump
/// producers emit real arrays; parsing only these columns keeps both forms
/// identical downstream (unparseable values are left as-is so the importer's
/// fail-closed checks still reject them).
const API_KEY_JSON_LIST_COLUMNS: &[&str] = &[
    "allowed_models",
    "blocked_models",
    "allowed_combos",
    "allowed_connections",
    "allowed_quotas",
    "allowed_endpoints",
    "ip_allowlist",
    "rate_limits",
    "scopes",
    "preferred_connections",
];

fn json_list(value: &Value) -> Value {
    match value
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
    {
        Some(parsed) if parsed.is_array() => parsed,
        _ => value.clone(),
    }
}

fn rows(root: &Value, table: &str) -> Result<Vec<Value>, String> {
    let Some(value) = root.get(table) else {
        return Ok(Vec::new());
    };
    let rows = value
        .as_array()
        .ok_or_else(|| format!("{table} must be an array"))?;
    rows.iter()
        .map(|row| {
            let fields = row
                .as_object()
                .ok_or_else(|| format!("{table} rows must be objects"))?;
            let mut normalized = Map::new();
            for (column, value) in fields {
                let value = if table == "api_keys"
                    && API_KEY_JSON_LIST_COLUMNS.contains(&column.as_str())
                {
                    json_list(value)
                } else {
                    value.clone()
                };
                let (name, value) = match column.as_str() {
                    "is_active" => {
                        let active = match value {
                            Value::Bool(active) => active,
                            value if value.as_i64() == Some(0) => false,
                            value if value.as_i64() == Some(1) => true,
                            _ => return Err(format!("{table}.is_active must be boolean or 0/1")),
                        };
                        ("isActive".into(), Value::Bool(active))
                    }
                    "custom_headers_json" => {
                        ("customHeaders".into(), json_object(&value, table, column)?)
                    }
                    "provider_specific_data" => (
                        "providerSpecificData".into(),
                        json_object(&value, table, column)?,
                    ),
                    "rate_limit_overrides_json" => (
                        "rateLimitOverrides".into(),
                        json_object(&value, table, column)?,
                    ),
                    _ => (camel_case(column), value),
                };
                normalized.insert(name, value);
            }
            Ok(Value::Object(normalized))
        })
        .collect()
}

pub(super) fn normalize(root: &Value) -> Result<Value, String> {
    let aliases = if let Some(rows) = root["key_value"].as_array() {
        let mut aliases = Map::new();
        for row in rows.iter().filter(|row| row["namespace"] == "modelAliases") {
            if let Some(key) = row["key"].as_str() {
                let value = row["value"]
                    .as_str()
                    .and_then(|value| serde_json::from_str::<Value>(value).ok())
                    .unwrap_or(Value::Null);
                aliases.insert(key.into(), value);
            }
        }
        Value::Object(aliases)
    } else {
        root["key_value"]
            .get("modelAliases")
            .cloned()
            .unwrap_or(json!({}))
    };
    Ok(json!({
        "_meta": {"version": "omniroute-v3-legacy-export"},
        "providerConnections": rows(root, "provider_connections")?,
        "providerNodes": rows(root, "provider_nodes")?,
        "apiKeys": rows(root, "api_keys")?,
        "combos": rows(root, "combos")?,
        "modelAliases": aliases,
        "modelComboMappings": rows(root, "model_combo_mappings")?,
        "modelContextOverrides": root["model_context_overrides"],
        "modelCapabilityOverrides": root["model_capability_overrides"],
    }))
}
