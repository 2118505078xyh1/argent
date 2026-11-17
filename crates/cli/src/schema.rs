//! 最小 JSON Schema 校验，用于 `--output-schema` / `--json-schema`。
//!
//! 只覆盖结构化输出真正用得到的子集：`type`、`properties`、`required`、
//! `additionalProperties`、`items`、`enum`。未知关键字一律放行——这是
//! 一个输出闸门，不是通用校验器；宁可少拦，也不要误伤一份合法的 schema。

use argent_core::json::Json;

/// 校验 `value` 是否符合 `schema`；错误信息带 JSON 路径，方便模型自我纠正。
pub fn validate(schema: &Json, value: &Json) -> Result<(), String> {
    check(schema, value, "$")
}

fn check(schema: &Json, value: &Json, path: &str) -> Result<(), String> {
    // `true`、`{}` 之类的 schema 接受任何值。
    if !matches!(schema, Json::Obj(_)) {
        return Ok(());
    }

    if let Some(Json::Arr(options)) = schema.get("enum") {
        if !options.iter().any(|option| option == value) {
            return Err(format!(
                "{}: value {:?} is not one of the allowed enum values",
                path,
                value.dump()
            ));
        }
    }

    if let Some(kind) = schema.get("type") {
        let names: Vec<&str> = match kind {
            Json::Str(name) => vec![name.as_str()],
            Json::Arr(items) => items.iter().filter_map(Json::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|name| matches_type(name, value)) {
            return Err(format!(
                "{}: expected {}, got {}",
                path,
                names.join(" | "),
                kind_of(value)
            ));
        }
    }

    if let Json::Obj(entries) = value {
        if let Some(Json::Arr(required)) = schema.get("required") {
            for name in required.iter().filter_map(Json::as_str) {
                if !entries.iter().any(|(key, _)| key == name) {
                    return Err(format!("{}: missing required property '{}'", path, name));
                }
            }
        }
        let properties = match schema.get("properties") {
            Some(Json::Obj(properties)) => Some(properties),
            _ => None,
        };
        let closed = matches!(schema.get("additionalProperties"), Some(Json::Bool(false)));
        for (key, child) in entries {
            match properties.and_then(|properties| {
                properties
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, schema)| schema)
            }) {
                Some(child_schema) => check(child_schema, child, &format!("{}.{}", path, key))?,
                None if closed => {
                    return Err(format!("{}: unexpected property '{}'", path, key));
                }
                None => {}
            }
        }
    }

    if let (Json::Arr(items), Some(item_schema)) = (value, schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            check(item_schema, item, &format!("{}[{}]", path, index))?;
        }
    }

    Ok(())
}

fn matches_type(name: &str, value: &Json) -> bool {
    match name {
        "object" => matches!(value, Json::Obj(_)),
        "array" => matches!(value, Json::Arr(_)),
        "string" => matches!(value, Json::Str(_)),
        "number" => matches!(value, Json::Num(_)),
        "integer" => matches!(value, Json::Num(n) if n.fract() == 0.0),
        "boolean" => matches!(value, Json::Bool(_)),
        "null" => matches!(value, Json::Null),
        // 未知关键字不拦截，向前兼容新草案。
        _ => true,
    }
}

fn kind_of(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Num(n) if n.fract() == 0.0 => "integer",
        Json::Num(_) => "number",
        Json::Str(_) => "string",
        Json::Arr(_) => "array",
        Json::Obj(_) => "object",
        Json::Raw(_) => "raw",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(text: &str) -> Json {
        Json::parse(text).expect("schema")
    }

    fn value(text: &str) -> Json {
        Json::parse(text).expect("value")
    }

    #[test]
    fn accepts_a_matching_object() {
        let s = schema(
            r#"{"type":"object","required":["title","tags"],
                "properties":{"title":{"type":"string"},
                              "tags":{"type":"array","items":{"type":"string"}}}}"#,
        );
        let v = value(r#"{"title":"ok","tags":["a","b"]}"#);
        assert!(validate(&s, &v).is_ok());
    }

    #[test]
    fn reports_the_path_of_a_bad_nested_value() {
        let s = schema(
            r#"{"type":"object","properties":{"items":{"type":"array",
                "items":{"type":"object","required":["id"],
                         "properties":{"id":{"type":"integer"}}}}}}"#,
        );
        let error =
            validate(&s, &value(r#"{"items":[{"id":1},{"id":"x"}]}"#)).expect_err("must reject");
        assert!(error.contains("$.items[1].id"), "{}", error);
    }

    #[test]
    fn enforces_required_and_closed_objects() {
        let s = schema(
            r#"{"type":"object","required":["a"],"additionalProperties":false,
                "properties":{"a":{"type":"number"}}}"#,
        );
        assert!(validate(&s, &value(r#"{"a":1}"#)).is_ok());
        assert!(validate(&s, &value(r#"{"b":1}"#)).is_err());
        assert!(validate(&s, &value(r#"{"a":1,"b":2}"#)).is_err());
    }

    #[test]
    fn enforces_enums_and_integer_types() {
        let s = schema(
            r#"{"type":"object","properties":{"state":{"enum":["ok","bad"]},
                            "count":{"type":"integer"}}}"#,
        );
        assert!(validate(&s, &value(r#"{"state":"ok","count":2}"#)).is_ok());
        assert!(validate(&s, &value(r#"{"state":"maybe"}"#)).is_err());
        assert!(validate(&s, &value(r#"{"count":2.5}"#)).is_err());
    }

    #[test]
    fn unknown_keywords_do_not_reject_valid_output() {
        // `$defs`/`description` 之类的关键字交给调用方，校验器只放行。
        let s = schema(r#"{"type":"object","$defs":{"x":{"type":"string"}}}"#);
        assert!(validate(&s, &value(r#"{"anything":true}"#)).is_ok());
    }
}
