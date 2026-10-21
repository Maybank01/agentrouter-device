//! Codes from the one error catalog (Cloud `contracts/errors/catalog.json`, #2449; owner 2026-10-21
//! 「还有，错误那一块，是不是应该有一种统一的方式？我不想错误都散落在各处」). The pinned copy is
//! `error-catalog.json` (`scripts/sync-error-catalog.mjs`); Control fills in component, retry and next
//! from it, and the device's own words travel whole as the cause. Never a bare `FAILED`.

/// The arguments of a call are not what the tool takes; the message names the argument and what it must be.
pub const HOSTD_ARGS_INVALID: &str = "HOSTD_ARGS_INVALID";
/// The write boundary (Windows integrity label) could not be set, so the command did not run.
pub const HOSTD_BOUNDARY_FAILED: &str = "HOSTD_BOUNDARY_FAILED";
/// A fault on this computer (a file could not be written, …); retrying will not help.
pub const HOSTD_FAULT: &str = "HOSTD_FAULT";

/// The source the pinned catalog copy was taken from (`agentrouter-cloud@<commit>`).
pub fn catalog_source() -> String {
    let catalog: serde_json::Value =
        serde_json::from_str(include_str!("error-catalog.json")).unwrap_or_default();
    catalog["source"].as_str().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_is_in_the_pinned_catalog() {
        let catalog: serde_json::Value =
            serde_json::from_str(include_str!("error-catalog.json")).unwrap();
        let codes: Vec<&str> = catalog["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["code"].as_str())
            .collect();
        for code in [HOSTD_ARGS_INVALID, HOSTD_BOUNDARY_FAILED, HOSTD_FAULT] {
            assert!(codes.contains(&code), "{code} is not in the pinned catalog");
        }
        assert!(catalog_source().starts_with("agentrouter-cloud@"));
    }
}
