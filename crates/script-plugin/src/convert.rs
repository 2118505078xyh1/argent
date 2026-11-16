//! Convert between argent's zero-dependency `Json` and Rhai's `Dynamic`.
//!
//! Maps become `rhai::Map`, arrays become `rhai::Array`, numbers become `f64`.
//! The conversion is lossless for the JSON subset argent uses in tool
//! arguments and plugin declarations.

use argent_core::json::Json;
use rhai::{Array, Dynamic, Map};

pub fn json_to_dynamic(value: &Json) -> Dynamic {
    match value {
        Json::Null => Dynamic::UNIT,
        Json::Bool(flag) => Dynamic::from(*flag),
        Json::Num(number) => Dynamic::from(*number),
        Json::Str(text) => Dynamic::from(text.clone()),
        Json::Arr(items) => {
            let array: Array = items.iter().map(json_to_dynamic).collect();
            Dynamic::from(array)
        }
        Json::Obj(entries) => {
            let mut map: Map = Map::new();
            for (key, value) in entries {
                map.insert(key.clone().into(), json_to_dynamic(value));
            }
            Dynamic::from(map)
        }
        Json::Raw(text) => Dynamic::from(text.clone()),
    }
}

pub fn dynamic_to_json(value: &Dynamic) -> Json {
    if let Some(map) = value.clone().try_cast::<Map>() {
        let mut entries = Vec::new();
        for (key, child) in map {
            entries.push((key.to_string(), dynamic_to_json(&child)));
        }
        return Json::Obj(entries);
    }
    if let Some(array) = value.clone().try_cast::<Array>() {
        return Json::Arr(array.iter().map(dynamic_to_json).collect());
    }
    if let Some(flag) = value.clone().try_cast::<bool>() {
        return Json::Bool(flag);
    }
    if let Some(number) = value.clone().try_cast::<f64>() {
        return Json::Num(number);
    }
    if let Some(number) = value.clone().try_cast::<i64>() {
        return Json::Num(number as f64);
    }
    if let Some(text) = value.clone().try_cast::<String>() {
        return Json::Str(text);
    }
    if value.is_unit() {
        return Json::Null;
    }
    Json::Str(value.to_string())
}
