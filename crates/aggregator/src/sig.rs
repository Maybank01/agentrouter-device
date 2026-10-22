//! The link request signature v3 (docs/HOSTD-MCP.md §3.3): only the control plane can make a request the device
//! acts on, carried in MCP's own extension point `params._meta["agentrouter.top/sig"]`:
//!
//! ```text
//! {"v": 3, "kid", "link", "session", "digest", "exp", "nonce", "sig"}
//! signed string: agentrouter-link-request/v3\n<link>\n<session>\n<method>\n<digest>\n<exp>\n<nonce>
//! digest:        sha256 hex of the canonical JSON of params without `_meta`
//! ```
//!
//! Canonical JSON, the key format, the 60 s (+5 s skew) expiry window and the replay cache are the ones of the
//! device protocol (`agentrouter_device::protocol`). Signed: `initialize` (binds the MCP session to the
//! conversation) and every `tools/call`.

use serde_json::{Map, Value};

use agentrouter_device::protocol::{
    ControlKey, REQUEST_MAX_AHEAD_MS, ReplayCache, args_digest, verify,
};

/// The `_meta` key of the signature.
pub const META_KEY: &str = "agentrouter.top/sig";
pub const VERSION: i64 = 3;

/// The methods that must carry a signature.
pub fn must_be_signed(method: &str) -> bool {
    matches!(method, "initialize" | "tools/call")
}

pub fn signed_string(
    link: &str,
    session: &str,
    method: &str,
    digest: &str,
    exp: i64,
    nonce: &str,
) -> String {
    format!("agentrouter-link-request/v3\n{link}\n{session}\n{method}\n{digest}\n{exp}\n{nonce}")
}

/// The digest of a request's params: canonical JSON without `_meta`.
pub fn params_digest(params: &Value) -> String {
    let mut map = params.as_object().cloned().unwrap_or_else(Map::new);
    map.remove("_meta");
    args_digest(&Value::Object(map))
}

/// The `initialize` param that names the account the session's calls are launched under (DEVICES §3.4). It is a
/// top-level param, so the v3 digest signs it.
pub const ACCOUNT_PARAM: &str = "agentrouter.top/account";

/// The `initialize` param that names the person's own recipe the session runs (`<space>/<path>@<version>`; absent for
/// the Main Agent and platform Agents), signed the same way: checked against the owner's rules (DEVICES §3.1).
pub const RECIPE_PARAM: &str = "agentrouter.top/recipe";

/// The string the control plane signs to tell a link whose it is (the gateway's `welcome` carries it as `owner`):
///
/// ```text
/// {"v": 1, "kid", "link", "account", "sig"}
/// signed string: agentrouter-link-owner/v1\n<link>\n<account>
/// ```
pub fn owner_string(link: &str, account: &str) -> String {
    format!("agentrouter-link-owner/v1\n{link}\n{account}")
}

/// The account a link belongs to, from the control plane's signed owner statement: checked against the key pinned at
/// pairing and this link's id.
pub fn check_owner(statement: &Value, link: &str, key: &ControlKey) -> Result<String, Refused> {
    let field = |name: &str| statement.get(name).and_then(Value::as_str).unwrap_or("");
    if statement.get("v").and_then(Value::as_i64) != Some(1) {
        return Err(refused("SIGNATURE_INVALID", "主人声明的版本不对"));
    }
    if field("kid") != key.kid {
        return Err(refused("SIGNATURE_INVALID", "主人声明用的是不认识的密钥"));
    }
    let (signed_link, account) = (field("link"), field("account"));
    if !verify(
        &key.public_key,
        &owner_string(signed_link, account),
        field("sig"),
    ) {
        return Err(refused("SIGNATURE_INVALID", "主人声明的签名不对"));
    }
    if signed_link != link {
        return Err(refused("WRONG_LINK", "主人声明是签给另一个链接的"));
    }
    if account.is_empty() || account.len() > 64 {
        return Err(refused("SIGNATURE_INVALID", "主人声明没有账号"));
    }
    Ok(account.to_string())
}

/// What a valid signature says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// The conversation (`ags_…`) the control plane signed for.
    pub session: String,
    pub nonce: String,
}

/// Why a request was refused: the code (`data.code` of the JSON-RPC error) and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub code: &'static str,
    pub message: String,
}

fn refused(code: &'static str, message: impl Into<String>) -> Refused {
    Refused {
        code,
        message: message.into(),
    }
}

/// Check one request's signature, in the order of the device protocol (§5.1 there): version, key, signature,
/// link, digest, expiry, nonce, replay.
pub fn check(
    method: &str,
    params: &Value,
    link: &str,
    key: &ControlKey,
    now: i64,
    replay: &mut ReplayCache,
) -> Result<Signed, Refused> {
    let Some(sig) = params.get("_meta").and_then(|m| m.get(META_KEY)) else {
        return Err(refused("SIGNATURE_INVALID", "没有签名"));
    };
    let field = |name: &str| sig.get(name).and_then(Value::as_str).unwrap_or("");
    if sig.get("v").and_then(Value::as_i64) != Some(VERSION) {
        return Err(refused("SIGNATURE_INVALID", "签名版本不对"));
    }
    if field("kid") != key.kid {
        return Err(refused("SIGNATURE_INVALID", "不认识的签名密钥"));
    }
    let Some(exp) = sig.get("exp").and_then(Value::as_i64) else {
        return Err(refused("SIGNATURE_INVALID", "签名没有有效期"));
    };
    let (signed_link, session, digest, nonce) = (
        field("link"),
        field("session"),
        field("digest"),
        field("nonce"),
    );
    let text = signed_string(signed_link, session, method, digest, exp, nonce);
    if !verify(&key.public_key, &text, field("sig")) {
        return Err(refused("SIGNATURE_INVALID", "签名不对"));
    }
    if signed_link != link {
        return Err(refused("WRONG_LINK", "这个请求是签给另一个链接的"));
    }
    if params_digest(params) != digest {
        return Err(refused("SIGNATURE_INVALID", "参数和签名对不上"));
    }
    if exp <= now {
        return Err(refused("REQUEST_EXPIRED", "请求已过期"));
    }
    if exp > now + REQUEST_MAX_AHEAD_MS {
        return Err(refused("REQUEST_EXPIRED", "请求的有效期太长"));
    }
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(refused("SIGNATURE_INVALID", "nonce 格式不对"));
    }
    if session.is_empty() {
        return Err(refused("SIGNATURE_INVALID", "签名没有会话"));
    }
    if !replay.remember(nonce, exp, now) {
        return Err(refused("REPLAYED", "这个请求已经用过"));
    }
    Ok(Signed {
        session: session.to_string(),
        nonce: nonce.to_string(),
    })
}
