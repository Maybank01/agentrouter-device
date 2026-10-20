//! Byte-for-byte agreement with the control plane: vectors made by `tests/fixtures/make-protocol-vectors.mjs`
//! with the same canonical JSON and Node's Ed25519 as agentrouter-cloud `linked-devices/protocol.ts`.

use agentrouter_device::protocol::{
    ControlKey, DeviceKey, ReplayCache, args_digest, canonical_json, js_number, verify,
};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("fixtures/protocol-vectors.json")).unwrap()
}

#[test]
fn canonical_json_and_digest_match_the_control_plane() {
    let v = vectors();
    let args: Value = serde_json::from_str(v["argsText"].as_str().unwrap()).unwrap();
    assert_eq!(canonical_json(&args), v["canon"].as_str().unwrap());
    assert_eq!(args_digest(&args), v["digest"].as_str().unwrap());
}

#[test]
fn numbers_print_like_javascript() {
    for pair in vectors()["numbers"].as_array().unwrap() {
        let (text, want) = (pair[0].as_str().unwrap(), pair[1].as_str().unwrap());
        let n: serde_json::Number = serde_json::from_str(text).unwrap();
        assert_eq!(js_number(&n), want, "{text}");
    }
}

#[test]
fn keys_and_signatures_match_node() {
    let v = vectors();
    let seed: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&v["seedHex"].as_str().unwrap()[i * 2..i * 2 + 2], 16).unwrap())
        .collect();
    let key = DeviceKey::from_seed(seed.try_into().unwrap());
    assert_eq!(key.public_key(), v["pub"].as_str().unwrap());
    let control = ControlKey {
        kid: v["kid"].as_str().unwrap().to_string(),
        public_key: v["pub"].as_str().unwrap().to_string(),
    };
    assert!(control.is_consistent());
    // Node's signature over a signed string verifies here (the same Ed25519 the link signature v3 uses).
    let request = &v["request"];
    let field = |k: &str| request[k].as_str().unwrap().to_string();
    let text = format!(
        "agentrouter-device-request/v1\n{}\n{}\n{}\n{}\n{}\n{}",
        field("device"),
        field("session"),
        field("action"),
        field("digest"),
        request["exp"].as_i64().unwrap(),
        field("nonce")
    );
    assert!(verify(&control.public_key, &text, &field("sig")));
    assert!(!verify(
        &control.public_key,
        &format!("{text}x"),
        &field("sig")
    ));
    let exp = request["exp"].as_i64().unwrap();
    let mut replay = ReplayCache::default();
    assert!(replay.remember(&field("nonce"), exp, exp - 30_000));
    assert!(
        !replay.remember(&field("nonce"), exp, exp - 30_000),
        "a replay"
    );
}
