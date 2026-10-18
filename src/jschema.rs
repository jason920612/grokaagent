//! A JSON Schema subset, enough to check a workflow agent's structured
//! result: `type` (one or a list), `properties`, `required`,
//! `additionalProperties`, `items`, `enum`, `const`, `anyOf` / `oneOf` /
//! `allOf`, `minItems` / `maxItems`, `minLength` / `maxLength`, `minimum` /
//! `maximum`. Keywords outside the subset are ignored, never rejected, so a
//! schema written for a fuller validator still checks what it can here.

use serde_json::Value;

/// Problems with `value` against `schema`, each prefixed with its JSON path
/// (`$` is the root). Empty = the value fits.
pub fn validate(value: &Value, schema: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check(value, schema, "$", &mut errors);
    errors
}

/// Problems with the schema itself, as far as the subset reads it. A result
/// schema must describe an object: the agent hands it in as a tool call's
/// arguments.
pub fn check_result_schema(schema: &Value) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        return Err("schema must be a JSON object".into());
    };
    match obj.get("type") {
        Some(Value::String(t)) if t == "object" => {}
        None if obj.contains_key("properties") => {}
        _ => return Err("schema must have type \"object\" at its root".into()),
    }
    if let Some(p) = obj.get("properties") {
        if !p.is_object() {
            return Err("schema properties must be an object".into());
        }
    }
    if let Some(r) = obj.get("required") {
        if !r.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
            return Err("schema required must be a list of property names".into());
        }
    }
    Ok(())
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_type(v: &Value, t: &str) -> bool {
    match t {
        "number" => v.is_number(),
        // 3.0 is an integer too.
        "integer" => v.as_f64().is_some_and(|f| f.fract() == 0.0),
        other => type_name(v) == other,
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(60).collect::<String>())
    } else {
        s
    }
}

fn check(v: &Value, schema: &Value, at: &str, errors: &mut Vec<String>) {
    let Some(s) = schema.as_object() else {
        // `true` / `{}` accept anything; `false` accepts nothing.
        if schema == &Value::Bool(false) {
            errors.push(format!("{at}: no value is allowed here"));
        }
        return;
    };
    if let Some(t) = s.get("type") {
        let names: Vec<&str> = match t {
            Value::String(one) => vec![one.as_str()],
            Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|n| is_type(v, n)) {
            errors.push(format!("{at}: expected {}, got {}", names.join(" or "), type_name(v)));
            return;
        }
    }
    if let Some(Value::Array(options)) = s.get("enum") {
        if !options.contains(v) {
            let list: Vec<String> = options.iter().map(short).collect();
            errors.push(format!("{at}: {} is not one of {}", short(v), list.join(", ")));
        }
    }
    if let Some(c) = s.get("const") {
        if c != v {
            errors.push(format!("{at}: must be {}", short(c)));
        }
    }
    if let Some(Value::Array(all)) = s.get("allOf") {
        for sub in all {
            check(v, sub, at, errors);
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(options)) = s.get(key) {
            let fits = options.iter().filter(|sub| validate(v, sub).is_empty()).count();
            let ok = if key == "anyOf" { fits >= 1 } else { fits == 1 };
            if !ok {
                let first: Vec<String> = options
                    .first()
                    .map(|sub| validate(v, sub))
                    .unwrap_or_default();
                let why = first.first().map(|e| format!(" (first option: {e})")).unwrap_or_default();
                errors.push(format!(
                    "{at}: matches {fits} of the {key} options, needs {}{why}",
                    if key == "anyOf" { "at least one" } else { "exactly one" }
                ));
            }
        }
    }
    match v {
        Value::Object(map) => {
            if let Some(Value::Array(req)) = s.get("required") {
                for name in req.iter().filter_map(Value::as_str) {
                    if !map.contains_key(name) {
                        errors.push(format!("{at}: missing required property \"{name}\""));
                    }
                }
            }
            let props = s.get("properties").and_then(Value::as_object);
            for (k, item) in map {
                let path = format!("{at}.{k}");
                match props.and_then(|p| p.get(k)) {
                    Some(sub) => check(item, sub, &path, errors),
                    None => match s.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            errors.push(format!("{at}: unexpected property \"{k}\""))
                        }
                        Some(sub @ Value::Object(_)) => check(item, sub, &path, errors),
                        _ => {}
                    },
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = s.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min {
                    errors.push(format!("{at}: needs at least {min} items, has {}", items.len()));
                }
            }
            if let Some(max) = s.get("maxItems").and_then(Value::as_u64) {
                if items.len() as u64 > max {
                    errors.push(format!("{at}: allows at most {max} items, has {}", items.len()));
                }
            }
            if let Some(sub) = s.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(item, sub, &format!("{at}[{i}]"), errors);
                }
            }
        }
        Value::String(text) => {
            let n = text.chars().count() as u64;
            if let Some(min) = s.get("minLength").and_then(Value::as_u64) {
                if n < min {
                    errors.push(format!("{at}: needs at least {min} characters"));
                }
            }
            if let Some(max) = s.get("maxLength").and_then(Value::as_u64) {
                if n > max {
                    errors.push(format!("{at}: allows at most {max} characters"));
                }
            }
        }
        Value::Number(num) => {
            let f = num.as_f64().unwrap_or(0.0);
            if let Some(min) = s.get("minimum").and_then(Value::as_f64) {
                if f < min {
                    errors.push(format!("{at}: must be ≥ {min}"));
                }
            }
            if let Some(max) = s.get("maximum").and_then(Value::as_f64) {
                if f > max {
                    errors.push(format!("{at}: must be ≤ {max}"));
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bugs() -> Value {
        json!({
            "type": "object",
            "properties": {
                "bugs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": {"type": "string", "minLength": 1},
                            "line": {"type": "integer", "minimum": 1},
                            "severity": {"enum": ["low", "high"]}
                        },
                        "required": ["file", "line"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["bugs"]
        })
    }

    #[test]
    fn a_fitting_value_has_no_errors() {
        let v = json!({"bugs": [{"file": "a.rs", "line": 3, "severity": "high"}]});
        assert!(validate(&v, &bugs()).is_empty());
        // 3.0 counts as an integer.
        let v = json!({"bugs": [{"file": "a.rs", "line": 3.0}]});
        assert!(validate(&v, &bugs()).is_empty());
    }

    #[test]
    fn errors_name_the_path_and_the_problem() {
        let v = json!({"bugs": [{"file": "", "line": 0, "severity": "mid", "extra": 1}, {"line": "x"}]});
        let errs = validate(&v, &bugs());
        let all = errs.join("\n");
        assert!(all.contains("$.bugs[0].file: needs at least 1 characters"), "{all}");
        assert!(all.contains("$.bugs[0].line: must be ≥ 1"), "{all}");
        assert!(all.contains("$.bugs[0].severity: \"mid\" is not one of"), "{all}");
        assert!(all.contains("$.bugs[0]: unexpected property \"extra\""), "{all}");
        assert!(all.contains("$.bugs[1]: missing required property \"file\""), "{all}");
        assert!(all.contains("$.bugs[1].line: expected integer, got string"), "{all}");
        assert!(validate(&json!({}), &bugs())[0].contains("missing required property \"bugs\""));
    }

    #[test]
    fn type_lists_and_combinators() {
        let s = json!({"type": ["string", "null"]});
        assert!(validate(&json!(null), &s).is_empty());
        assert!(!validate(&json!(1), &s).is_empty());
        let s = json!({"anyOf": [{"type": "string"}, {"type": "integer"}]});
        assert!(validate(&json!(2), &s).is_empty());
        assert!(!validate(&json!(true), &s).is_empty());
        let s = json!({"oneOf": [{"type": "number"}, {"type": "integer"}]});
        // 2 is both a number and an integer: not exactly one.
        assert!(!validate(&json!(2), &s).is_empty());
        assert!(validate(&json!(2.5), &s).is_empty());
        assert!(!validate(&json!(1), &json!(false)).is_empty());
        assert!(validate(&json!(1), &json!(true)).is_empty());
    }

    #[test]
    fn unknown_keywords_are_ignored() {
        let s = json!({"type": "string", "format": "email", "pattern": "^x"});
        assert!(validate(&json!("anything"), &s).is_empty());
    }

    #[test]
    fn result_schemas_must_describe_an_object() {
        assert!(check_result_schema(&bugs()).is_ok());
        assert!(check_result_schema(&json!({"properties": {}})).is_ok());
        assert!(check_result_schema(&json!({"type": "array"})).is_err());
        assert!(check_result_schema(&json!("x")).is_err());
        assert!(check_result_schema(&json!({"type": "object", "required": [1]})).is_err());
    }
}
