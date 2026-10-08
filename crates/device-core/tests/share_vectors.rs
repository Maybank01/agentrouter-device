//! Byte-for-byte agreement with the share-code contract (DEVICE-PROTOCOL.md §11.2, §11.3, §13.1):
//! `fixtures/share-vectors.json` is agentrouter-cloud
//! `services/control-plane/src/linked-devices/share-vectors.json`, copied unchanged.

use agentrouter_device::protocol::{
    ControlKey, DeviceKey, ReplayCache, args_digest, check_request, request_string_v2, verify,
};
use agentrouter_device::share::{
    ShareTerms, cap_body, cap_string, normalise_password, open_body, password_body,
    password_string, share_string, stop_body, stop_string,
};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("fixtures/share-vectors.json")).unwrap()
}

fn key(v: &Value) -> DeviceKey {
    let hex = v["device"]["seedHex"].as_str().unwrap();
    let seed: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect();
    DeviceKey::from_seed(seed.try_into().unwrap())
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[test]
fn password_normalisation() {
    for p in vectors()["passwords"].as_array().unwrap() {
        assert_eq!(
            normalise_password(s(&p["input"])).as_deref(),
            p["normalised"].as_str(),
            "{p}"
        );
    }
}

#[test]
fn the_four_device_signatures() {
    let v = vectors();
    let key = key(&v);
    assert_eq!(key.public_key(), s(&v["device"]["publicKey"]));
    let device = s(&v["device"]["id"]);

    let body = &v["share"]["body"];
    let terms: ShareTerms = serde_json::from_value(body.clone()).unwrap();
    let at = body["at"].as_i64().unwrap();
    let password = s(&body["password"]);
    let text = share_string(device, &terms, password, at);
    assert_eq!(text, s(&v["share"]["text"]));
    assert_eq!(key.sign(&text), s(&v["share"]["signature"]));
    let made = open_body(&key, device, &terms, password, at);
    for field in [
        "share",
        "access",
        "folders",
        "expiresAt",
        "approval",
        "modes",
        "delegateCapPoints",
        "password",
        "at",
    ] {
        assert_eq!(made[field], body[field], "{field}");
    }
    assert_eq!(made["signature"], v["share"]["signature"]);
    assert_eq!(terms.problem(at), None);

    let share = s(&body["share"]);
    let b = &v["password"]["body"];
    let text = password_string(device, share, s(&b["password"]), b["at"].as_i64().unwrap());
    assert_eq!(text, s(&v["password"]["text"]));
    let made = password_body(
        &key,
        device,
        share,
        s(&b["password"]),
        b["at"].as_i64().unwrap(),
    );
    assert_eq!(made["signature"], v["password"]["signature"]);

    let b = &v["stop"]["body"];
    let text = stop_string(device, share, s(&b["reason"]), b["at"].as_i64().unwrap());
    assert_eq!(text, s(&v["stop"]["text"]));
    let made = stop_body(
        &key,
        device,
        share,
        s(&b["reason"]),
        b["at"].as_i64().unwrap(),
    );
    assert_eq!(made["signature"], v["stop"]["signature"]);

    let b = &v["cap"]["body"];
    let add = b["addPoints"].as_i64().unwrap();
    let text = cap_string(device, share, add, b["at"].as_i64().unwrap());
    assert_eq!(text, s(&v["cap"]["text"]));
    let made = cap_body(&key, device, share, add, b["at"].as_i64().unwrap());
    assert_eq!(made["signature"], v["cap"]["signature"]);
    assert!(verify(
        &key.public_key(),
        s(&v["cap"]["text"]),
        s(&v["cap"]["signature"])
    ));
}

#[test]
fn request_v2_and_session_open_verify() {
    let v = vectors();
    let control = ControlKey {
        kid: s(&v["control"]["kid"]).to_string(),
        public_key: s(&v["control"]["publicKey"]).to_string(),
    };
    assert!(control.is_consistent());
    for name in ["requestV2", "sessionOpen"] {
        let vector = &v[name];
        let r = &vector["request"];
        assert_eq!(args_digest(&vector["args"]), s(&r["digest"]), "{name}");
        let text = request_string_v2(
            s(&r["device"]),
            s(&r["session"]),
            s(&r["share"]),
            s(&r["client"]),
            s(&r["action"]),
            s(&r["digest"]),
            r["exp"].as_i64().unwrap(),
            s(&r["nonce"]),
        );
        assert_eq!(text, s(&vector["text"]), "{name}");
        let mut replay = ReplayCache::default();
        let ok = check_request(
            r,
            &vector["args"],
            &control,
            s(&v["device"]["id"]),
            r["exp"].as_i64().unwrap() - 30_000,
            &mut replay,
        )
        .unwrap();
        assert_eq!(ok.share, s(&r["share"]));
        assert_eq!(ok.client, s(&r["client"]));
        assert_eq!(ok.action, s(&r["action"]));
    }
    // A v2 request with its share field changed no longer verifies.
    let mut forged = v["requestV2"]["request"].clone();
    forged["share"] = Value::String(format!("shr_{}", "e".repeat(32)));
    let err = check_request(
        &forged,
        &v["requestV2"]["args"],
        &control,
        s(&v["device"]["id"]),
        forged["exp"].as_i64().unwrap() - 30_000,
        &mut ReplayCache::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "SIGNATURE_INVALID");
}
