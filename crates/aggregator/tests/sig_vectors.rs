//! The link request signature v3 against the vectors shared with the Cloud devices module
//! (`link-sig-v3-vectors.json`, made by `scripts/make-link-sig-vectors.mjs`).

use agentrouter_aggregator::sig;
use agentrouter_device::protocol::{ControlKey, ReplayCache, canonical_json};
use serde_json::{Map, Value};

#[test]
fn every_vector_gives_the_expected_answer() {
    let v: Value = serde_json::from_str(include_str!("link-sig-v3-vectors.json")).unwrap();
    let key = ControlKey {
        kid: v["kid"].as_str().unwrap().into(),
        public_key: v["publicKey"].as_str().unwrap().into(),
    };
    assert!(key.is_consistent());
    let link = v["link"].as_str().unwrap();
    let now = v["nowMs"].as_i64().unwrap();
    let mut replay = ReplayCache::default();
    for case in v["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let method = case["method"].as_str().unwrap();
        let params = &case["params"];
        if let Some(canonical) = case["canonical"].as_str().filter(|c| !c.is_empty()) {
            let mut without = params.as_object().cloned().unwrap_or_else(Map::new);
            without.remove("_meta");
            if case["expect"] == "ok" {
                assert_eq!(
                    canonical_json(&Value::Object(without)),
                    canonical,
                    "{name}: canonical JSON"
                );
            }
        }
        let got = sig::check(method, params, link, &key, now, &mut replay);
        match case["expect"].as_str().unwrap() {
            "ok" => {
                let signed = got.unwrap_or_else(|e| panic!("{name}: {e:?}"));
                assert_eq!(signed.session, "ags_conv1", "{name}");
                let s = &params["_meta"][sig::META_KEY];
                assert_eq!(
                    sig::signed_string(
                        s["link"].as_str().unwrap(),
                        s["session"].as_str().unwrap(),
                        method,
                        s["digest"].as_str().unwrap(),
                        s["exp"].as_i64().unwrap(),
                        s["nonce"].as_str().unwrap()
                    ),
                    case["string"].as_str().unwrap(),
                    "{name}: signed string"
                );
            }
            code => assert_eq!(got.unwrap_err().code, code, "{name}"),
        }
    }
}

#[test]
fn only_initialize_and_tool_calls_need_a_signature() {
    assert!(sig::must_be_signed("initialize"));
    assert!(sig::must_be_signed("tools/call"));
    for m in [
        "tools/list",
        "ping",
        "notifications/initialized",
        "notifications/cancelled",
    ] {
        assert!(!sig::must_be_signed(m), "{m}");
    }
}
