//! Live Grok CLI model catalog (`GET /v1/models-v2`, fallback `/v1/models`).
//!
//! Parsing follows grok-build `parse_remote_model_value`: OpenAI `{data:[…]}`
//! wrapper, camelCase / snake_case / `_meta`, per-model `reasoningEfforts`.

use serde_json::{Map, Value};

use crate::provider::ReasoningEffort;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortOpt {
    pub id: String,
    pub value: ReasoningEffort,
    pub label: String,
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    pub supports_reasoning_effort: bool,
    pub default_effort: Option<ReasoningEffort>,
    pub efforts: Vec<EffortOpt>,
}

impl CatalogModel {
    /// Options shown in the effort dropdown. Empty means the picker is disabled
    /// and the request must not send `reasoning.effort`.
    pub fn picker_efforts(&self) -> Vec<EffortOpt> {
        if !self.supports_reasoning_effort {
            return Vec::new();
        }
        if !self.efforts.is_empty() {
            return self.efforts.clone();
        }
        self.default_effort
            .map(|v| {
                vec![EffortOpt {
                    id: v.as_str().to_string(),
                    value: v,
                    label: v.label().to_string(),
                    default: true,
                }]
            })
            .unwrap_or_default()
    }

    pub fn send_reasoning(&self) -> bool {
        self.supports_reasoning_effort && !self.picker_efforts().is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    pub models: Vec<CatalogModel>,
}

impl ModelCatalog {
    pub fn find(&self, model_id: &str) -> Option<&CatalogModel> {
        let needle = model_id.trim();
        self.models.iter().find(|m| m.id == needle)
    }

    pub fn picker_efforts(&self, model_id: &str) -> Vec<EffortOpt> {
        self.find(model_id)
            .map(CatalogModel::picker_efforts)
            .unwrap_or_default()
    }

    /// Keep the current selection visible even if the catalog omitted it.
    pub fn ensure_current(&mut self, model_id: &str, effort: ReasoningEffort) {
        let id = model_id.trim();
        if id.is_empty() || self.find(id).is_some() {
            return;
        }
        self.models.insert(
            0,
            CatalogModel {
                id: id.to_string(),
                name: id.to_string(),
                supports_reasoning_effort: true,
                default_effort: Some(effort),
                efforts: vec![EffortOpt {
                    id: effort.as_str().to_string(),
                    value: effort,
                    label: effort.label().to_string(),
                    default: true,
                }],
            },
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortChoice {
    pub effort: ReasoningEffort,
    pub send_reasoning: bool,
}

/// Clamp effort to the model's catalog list. If the model has no effort
/// picker, keep the previous value but do not send `reasoning.effort`.
pub fn clamp_effort_for_model(
    catalog: &ModelCatalog,
    model_id: &str,
    current: ReasoningEffort,
) -> EffortChoice {
    let Some(model) = catalog.find(model_id) else {
        return EffortChoice {
            effort: current,
            send_reasoning: true,
        };
    };
    let efforts = model.picker_efforts();
    if efforts.is_empty() {
        return EffortChoice {
            effort: current,
            send_reasoning: false,
        };
    }
    let effort = if efforts.iter().any(|e| e.value == current) {
        current
    } else {
        model
            .default_effort
            .or_else(|| efforts.iter().find(|e| e.default).map(|e| e.value))
            .unwrap_or(efforts[0].value)
    };
    EffortChoice {
        effort,
        send_reasoning: true,
    }
}

pub fn cycle_effort(
    efforts: &[EffortOpt],
    current: ReasoningEffort,
    back: bool,
) -> Option<ReasoningEffort> {
    if efforts.is_empty() {
        return None;
    }
    let i = efforts
        .iter()
        .position(|e| e.value == current)
        .unwrap_or(0);
    let next = if back {
        if i == 0 {
            efforts.len() - 1
        } else {
            i - 1
        }
    } else {
        (i + 1) % efforts.len()
    };
    Some(efforts[next].value)
}

/// Catalog GET URLs. Override is a full URL (`GROKA_MODELS_LIST_URL` /
/// `GROK_MODELS_LIST_URL`). Otherwise try `/models-v2` then `/models`.
pub fn catalog_urls(base_url: &str, override_url: Option<&str>) -> Vec<String> {
    if let Some(url) = override_url.map(str::trim).filter(|s| !s.is_empty()) {
        return vec![url.to_string()];
    }
    let base = base_url.trim_end_matches('/');
    vec![format!("{base}/models-v2"), format!("{base}/models")]
}

pub fn models_list_override_from_env() -> Option<String> {
    std::env::var("GROKA_MODELS_LIST_URL")
        .ok()
        .or_else(|| std::env::var("GROK_MODELS_LIST_URL").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn parse_catalog_json(text: &str) -> Result<ModelCatalog, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("model catalog is not JSON: {e}"))?;
    parse_catalog_value(&value)
}

pub fn parse_catalog_value(value: &Value) -> Result<ModelCatalog, String> {
    let rows = catalog_rows(value)
        .ok_or_else(|| "model catalog missing data array".to_string())?;
    let mut models = Vec::new();
    for row in rows {
        if let Some(model) = parse_model(row) {
            models.push(model);
        }
    }
    Ok(ModelCatalog { models })
}

fn catalog_rows(value: &Value) -> Option<&Vec<Value>> {
    if let Some(arr) = value.as_array() {
        return Some(arr);
    }
    let obj = value.as_object()?;
    obj.get("data")
        .and_then(Value::as_array)
        .or_else(|| obj.get("models").and_then(Value::as_array))
}

fn parse_model(value: &Value) -> Option<CatalogModel> {
    let obj = value.as_object()?;
    let meta = obj.get("_meta").and_then(Value::as_object);
    if get_bool(obj, meta, "hidden", "hidden").unwrap_or(false) {
        return None;
    }
    if !get_bool(obj, meta, "supportedInApi", "supported_in_api").unwrap_or(true) {
        return None;
    }
    let id = get_string(obj, "id")
        .or_else(|| get_string(obj, "model"))
        .or_else(|| get_string(obj, "modelId"))
        .or_else(|| meta.and_then(|m| get_string(m, "model")))
        .or_else(|| meta.and_then(|m| get_string(m, "modelId")))
        .filter(|s| !s.trim().is_empty())?;
    let name = get_string(obj, "name")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    let supports = get_bool(
        obj,
        meta,
        "supportsReasoningEffort",
        "supports_reasoning_effort",
    )
    .unwrap_or(false);
    let default_effort = get_string(obj, "reasoningEffort")
        .or_else(|| get_string(obj, "reasoning_effort"))
        .or_else(|| meta.and_then(|m| get_string(m, "reasoningEffort")))
        .or_else(|| meta.and_then(|m| get_string(m, "reasoning_effort")))
        .and_then(|s| ReasoningEffort::parse(&s));
    let efforts = get_array(obj, meta, "reasoningEfforts", "reasoning_efforts")
        .map(|arr| parse_effort_options(arr))
        .unwrap_or_default();
    Some(CatalogModel {
        id,
        name,
        supports_reasoning_effort: supports,
        default_effort,
        efforts,
    })
}

fn parse_effort_options(arr: &[Value]) -> Vec<EffortOpt> {
    arr.iter().filter_map(parse_effort_option).collect()
}

fn parse_effort_option(value: &Value) -> Option<EffortOpt> {
    if let Some(s) = value.as_str() {
        let effort = ReasoningEffort::parse(s)?;
        return Some(EffortOpt {
            id: effort.as_str().to_string(),
            value: effort,
            label: effort.label().to_string(),
            default: false,
        });
    }
    let obj = value.as_object()?;
    let raw = get_string(obj, "value")
        .or_else(|| get_string(obj, "id"))
        .or_else(|| get_string(obj, "effort"))?;
    let effort = ReasoningEffort::parse(&raw)?;
    let id = get_string(obj, "id").unwrap_or_else(|| effort.as_str().to_string());
    let label = get_string(obj, "label")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| effort.label().to_string());
    let default = obj.get("default").and_then(Value::as_bool).unwrap_or(false);
    Some(EffortOpt {
        id,
        value: effort,
        label,
        default,
    })
}

fn get_string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn get_bool(
    obj: &Map<String, Value>,
    meta: Option<&Map<String, Value>>,
    camel: &str,
    snake: &str,
) -> Option<bool> {
    obj.get(camel)
        .or_else(|| obj.get(snake))
        .or_else(|| meta.and_then(|m| m.get(camel).or_else(|| m.get(snake))))
        .and_then(Value::as_bool)
}

fn get_array<'a>(
    obj: &'a Map<String, Value>,
    meta: Option<&'a Map<String, Value>>,
    camel: &str,
    snake: &str,
) -> Option<&'a Vec<Value>> {
    obj.get(camel)
        .or_else(|| obj.get(snake))
        .or_else(|| meta.and_then(|m| m.get(camel).or_else(|| m.get(snake))))
        .and_then(Value::as_array)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "data": [
                {
                    "id": "grok-4.6",
                    "name": "Grok 4.6",
                    "hidden": false,
                    "supportsReasoningEffort": true,
                    "reasoningEffort": "high",
                    "reasoningEfforts": ["low", "medium", "high", "xhigh"]
                },
                {
                    "id": "grok-lite",
                    "name": "Grok Lite",
                    "supports_reasoning_effort": true,
                    "reasoning_effort": "high",
                    "reasoning_efforts": [
                        {"value": "low", "label": "Low"},
                        {"id": "hi", "value": "high", "label": "High", "default": true}
                    ]
                },
                {
                    "id": "no-think",
                    "name": "No Think",
                    "supportsReasoningEffort": false
                },
                {
                    "id": "hidden-model",
                    "hidden": true,
                    "supportsReasoningEffort": true,
                    "reasoningEfforts": ["low"]
                },
                {
                    "id": "api-off",
                    "supportedInApi": false
                },
                {
                    "_meta": {
                        "model": "meta-model",
                        "hidden": false,
                        "supportsReasoningEffort": true,
                        "reasoningEffort": "low",
                        "reasoningEfforts": ["low", "high"]
                    },
                    "name": "From Meta"
                },
                {
                    "id": "skip-unknown-effort",
                    "supportsReasoningEffort": true,
                    "reasoningEfforts": ["low", "quantum", "high"]
                },
                {
                    "id": ""
                }
            ]
        })
    }

    #[test]
    fn parses_openai_wrapper_and_skips_unusable() {
        let cat = parse_catalog_value(&fixture()).unwrap();
        let ids: Vec<&str> = cat.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "grok-4.6",
                "grok-lite",
                "no-think",
                "meta-model",
                "skip-unknown-effort"
            ]
        );
        assert!(cat.find("hidden-model").is_none());
        assert!(cat.find("api-off").is_none());
        let lite = cat.find("grok-lite").unwrap();
        assert_eq!(lite.name, "Grok Lite");
        assert_eq!(
            lite.efforts
                .iter()
                .map(|e| (e.value, e.label.as_str(), e.default))
                .collect::<Vec<_>>(),
            [
                (ReasoningEffort::Low, "Low", false),
                (ReasoningEffort::High, "High", true)
            ]
        );
        let meta = cat.find("meta-model").unwrap();
        assert_eq!(meta.name, "From Meta");
        assert_eq!(
            meta.picker_efforts()
                .iter()
                .map(|e| e.value)
                .collect::<Vec<_>>(),
            [ReasoningEffort::Low, ReasoningEffort::High]
        );
        let skip = cat.find("skip-unknown-effort").unwrap();
        assert_eq!(
            skip.efforts.iter().map(|e| e.value).collect::<Vec<_>>(),
            [ReasoningEffort::Low, ReasoningEffort::High]
        );
        assert!(!cat.find("no-think").unwrap().send_reasoning());
        assert!(cat.find("grok-4.6").unwrap().send_reasoning());
    }

    #[test]
    fn accepts_bare_array_and_models_key() {
        let bare = json!([{"id": "a", "supportsReasoningEffort": false}]);
        assert_eq!(parse_catalog_value(&bare).unwrap().models[0].id, "a");
        let wrapped = json!({"models": [{"id": "b"}]});
        assert_eq!(parse_catalog_value(&wrapped).unwrap().models[0].id, "b");
    }

    #[test]
    fn switching_model_clamps_illegal_effort_to_default() {
        let cat = parse_catalog_value(&fixture()).unwrap();
        let lite = clamp_effort_for_model(&cat, "grok-lite", ReasoningEffort::Xhigh);
        assert_eq!(lite.effort, ReasoningEffort::High);
        assert!(lite.send_reasoning);
        let none = clamp_effort_for_model(&cat, "no-think", ReasoningEffort::High);
        assert!(!none.send_reasoning);
        let keep = clamp_effort_for_model(&cat, "grok-4.6", ReasoningEffort::Low);
        assert_eq!(keep.effort, ReasoningEffort::Low);
        let unknown = clamp_effort_for_model(&cat, "not-in-catalog", ReasoningEffort::Medium);
        assert_eq!(unknown.effort, ReasoningEffort::Medium);
        assert!(unknown.send_reasoning);
    }

    #[test]
    fn cycle_stays_inside_catalog_list() {
        let cat = parse_catalog_value(&fixture()).unwrap();
        let efforts = cat.picker_efforts("grok-lite");
        assert_eq!(
            cycle_effort(&efforts, ReasoningEffort::Low, false),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            cycle_effort(&efforts, ReasoningEffort::High, false),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            cycle_effort(&efforts, ReasoningEffort::High, true),
            Some(ReasoningEffort::Low)
        );
        assert!(cycle_effort(&[], ReasoningEffort::High, false).is_none());
    }

    #[test]
    fn catalog_urls_try_v2_then_models_unless_overridden() {
        assert_eq!(
            catalog_urls("https://cli-chat-proxy.grok.com/v1/", None),
            [
                "https://cli-chat-proxy.grok.com/v1/models-v2",
                "https://cli-chat-proxy.grok.com/v1/models"
            ]
        );
        assert_eq!(
            catalog_urls("https://example/v1", Some(" https://custom/list ")),
            ["https://custom/list"]
        );
    }

    #[test]
    fn ensure_current_does_not_invent_hardcoded_tiers() {
        let mut cat = ModelCatalog::default();
        cat.ensure_current("mine", ReasoningEffort::Medium);
        assert_eq!(cat.models.len(), 1);
        assert_eq!(cat.models[0].id, "mine");
        assert_eq!(
            cat.picker_efforts("mine")
                .iter()
                .map(|e| e.value)
                .collect::<Vec<_>>(),
            [ReasoningEffort::Medium]
        );
        cat.ensure_current("mine", ReasoningEffort::High);
        assert_eq!(cat.models.len(), 1);
    }

    #[test]
    fn rejects_non_json() {
        assert!(parse_catalog_json("not-json").is_err());
        assert!(parse_catalog_value(&json!({"nope": 1})).is_err());
    }
}
