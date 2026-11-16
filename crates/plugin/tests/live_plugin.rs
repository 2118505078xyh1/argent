//! End-to-end test against the built demo plugin.
//!
//! Skipped when the cdylib has not been built, so `cargo test` alone stays
//! green on a clean checkout.

use std::path::PathBuf;

use argent_core::json::Json;
use argent_plugin::LoadedPlugin;

fn demo_library() -> Option<PathBuf> {
    // target/<profile>/argent_plugin_demo.{dll,so,dylib}, located relative to
    // this crate's manifest so it works from any workspace member.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.parent()?.parent()?;
    let mut candidates = Vec::new();
    for profile in ["release", "debug"] {
        let dir = root.join("target").join(profile);
        #[cfg(windows)]
        candidates.push(dir.join("argent_plugin_demo.dll"));
        #[cfg(target_os = "macos")]
        candidates.push(dir.join("libargent_plugin_demo.dylib"));
        #[cfg(all(unix, not(target_os = "macos")))]
        candidates.push(dir.join("libargent_plugin_demo.so"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

#[test]
fn loads_the_demo_plugin_and_calls_its_tools() {
    let path = match demo_library() {
        Some(path) => path,
        None => return,
    };
    let plugin = LoadedPlugin::load(&path).expect("load plugin");
    assert_eq!(plugin.name, "argent-plugin-demo");
    assert_eq!(plugin.version, "0.1.0");

    let tools = plugin.tools();
    assert_eq!(tools.len(), 2, "tools: {:?}", tools);
    assert_eq!(tools[0].name, "word_count");
    assert_eq!(tools[1].name, "reverse");

    let reply = plugin
        .call(tools[0].index, r#"{"text":"one two three"}"#)
        .expect("call word_count");
    let parsed = Json::parse(&reply).expect("parse reply");
    assert_eq!(parsed.str_or("content", ""), "3 words");
    assert!(!parsed.bool_or("isError", true));

    let reply = plugin
        .call(tools[1].index, r#"{"text":"abc"}"#)
        .expect("call reverse");
    let parsed = Json::parse(&reply).expect("parse reply");
    assert_eq!(parsed.str_or("content", ""), "cba");
}

#[test]
fn reports_a_missing_argument_through_the_abi() {
    let path = match demo_library() {
        Some(path) => path,
        None => return,
    };
    let plugin = LoadedPlugin::load(&path).expect("load plugin");
    let reply = plugin.call(0, r#"{"nope":true}"#).expect("call");
    let parsed = Json::parse(&reply).expect("parse reply");
    assert!(parsed.bool_or("isError", false));
}

#[test]
fn escapes_survive_the_round_trip() {
    // A payload with a quote proves both sides agree on the encoding of the
    // returned string.
    let path = match demo_library() {
        Some(path) => path,
        None => return,
    };
    let plugin = LoadedPlugin::load(&path).expect("load plugin");
    let args = Json::parse(r#"{"text":"a\"b"}"#).expect("args");
    let reply = plugin.call(1, &args.dump()).expect("call");
    let parsed = Json::parse(&reply).expect("parse reply");
    // "a\"b" reversed is `b"a`.
    assert_eq!(parsed.str_or("content", ""), "b\u{22}a");
}
