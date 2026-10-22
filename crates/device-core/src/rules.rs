//! The owner's usage rules for this computer (Cloud `docs/development/DEVICES.md` §3.1): which of the owner's Agents
//! may use it, and which of them each named local data is open to.
//!
//! Rules:
//! - **Set here**: `config.json` `rules`, written by `agentrouter rules set|reset` (later also by the web device page,
//!   through the link). The shape is the Cloud rules API's: `{"agents": "own" | ["<space>/<path>", …],
//!   "localData": {"<name>": ["<space>/<path>", …]}}` — a recipe any version (`@<version>` is ignored).
//! - **Reported as `link.rules`**: the cloud keeps a copy for placement (its "may" check).
//! - **Enforced here a second time** (`agentrouter_aggregator`): a session names the recipe it runs in its signed
//!   `initialize` (`agentrouter.top/recipe`, absent for the Main Agent); a recipe the rules do not admit is refused
//!   with `DEVICE_NOT_ALLOWED`, and a call that names a local data folder closed to it with `LOCAL_DATA_NOT_ALLOWED`.
//!   So a cloud-side mistake cannot widen this computer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The most recipes one list holds, and the most local data names with a list (the Cloud rules API's bounds).
pub const MAX_LIST: usize = 200;
const MAX_RECIPE: usize = 300;
const MAX_NAME: usize = 40;

/// Which of the owner's Agents may use this computer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Agents {
    /// Every instance launched under the owner's account (the default).
    #[default]
    Own,
    /// Only these recipes of the owner (`<space>/<path>`, any version); empty = none.
    Only(Vec<String>),
}

impl Serialize for Agents {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Agents::Own => s.serialize_str("own"),
            Agents::Only(list) => list.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Agents {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) if s == "own" => Ok(Agents::Own),
            Value::Array(items) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s),
                    _ => Err(serde::de::Error::custom(
                        "agents: a list of recipe references (text)",
                    )),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Agents::Only),
            _ => Err(serde::de::Error::custom(
                r#"agents: "own" or a list of recipe references"#,
            )),
        }
    }
}

/// The owner's rules (absent fields: the defaults — every own Agent, every local data open to them).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rules {
    pub agents: Agents,
    /// Per registered local data name: the recipes it is open to (absent: every allowed Agent; `[]`: none).
    #[serde(alias = "localData", skip_serializing_if = "BTreeMap::is_empty")]
    pub local_data: BTreeMap<String, Vec<String>>,
}

/// A recipe reference without its version (`w_tools/agents/剪辑@3` → `w_tools/agents/剪辑`), as the cloud compares them.
fn recipe_key(recipe: &str) -> &str {
    let r = recipe.trim();
    match r.rfind('@') {
        Some(at) if !r[at + 1..].contains('/') => &r[..at],
        _ => r,
    }
}

fn listed(list: &[String], recipe: Option<&str>) -> bool {
    recipe.is_some_and(|r| list.iter().any(|x| recipe_key(x) == recipe_key(r)))
}

impl Rules {
    /// Rules in the Cloud rules API's words, checked by its bounds; the person's words on what is wrong otherwise.
    pub fn parse(text: &str) -> Result<Rules, String> {
        let rules: Rules = serde_json::from_str(text).map_err(|e| format!("规则读不懂：{e}"))?;
        rules.problem().map_or(Ok(rules), Err)
    }

    /// Why these rules cannot be used, or `None` (the Cloud rules API's bounds, `parseDeviceRules`).
    pub fn problem(&self) -> Option<String> {
        let bad_list = |list: &[String]| {
            list.len() > MAX_LIST
                || list
                    .iter()
                    .any(|r| r.trim().is_empty() || r.chars().count() > MAX_RECIPE)
        };
        if let Agents::Only(list) = &self.agents
            && bad_list(list)
        {
            return Some(format!(
                "agents 最多 {MAX_LIST} 个 Agent（<空间>/<路径>），每个 1–{MAX_RECIPE} 个字"
            ));
        }
        if self.local_data.len() > MAX_LIST {
            return Some(format!("localData 最多 {MAX_LIST} 个名字"));
        }
        for (name, list) in &self.local_data {
            if name.trim().is_empty() || name.chars().count() > MAX_NAME {
                return Some(format!("localData 的名字要 1–{MAX_NAME} 个字：{name}"));
            }
            if bad_list(list) {
                return Some(format!(
                    "localData.{name} 最多 {MAX_LIST} 个 Agent（<空间>/<路径>），每个 1–{MAX_RECIPE} 个字"
                ));
            }
        }
        None
    }

    /// Why an instance running `recipe` (None: the Main Agent or a platform Agent) may not use this computer, or `None`.
    pub fn refuses(&self, recipe: Option<&str>) -> Option<String> {
        match &self.agents {
            Agents::Own => None,
            Agents::Only(list) if listed(list, recipe) => None,
            Agents::Only(list) => Some(format!(
                "这台电脑的主人只让 {} 用它{}",
                if list.is_empty() {
                    "（谁都不）".to_string()
                } else {
                    list.join("、")
                },
                recipe
                    .map(|r| format!("，不包括 {r}"))
                    .unwrap_or_else(|| "，不包括主 Agent".into())
            )),
        }
    }

    /// Whether the local data `name` is open to `recipe` (names compare trimmed and case-insensitively).
    pub fn data_open(&self, name: &str, recipe: Option<&str>) -> bool {
        let rule = self
            .local_data
            .iter()
            .find(|(k, _)| crate::local_data::same_name(k, name));
        rule.is_none_or(|(_, list)| listed(list, recipe))
    }

    /// `link.rules`: the Cloud rules API's shape.
    pub fn view(&self) -> Value {
        let mut v = json!({ "agents": self.agents });
        if !self.local_data.is_empty() {
            v["localData"] = json!(self.local_data);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_rules_api_shape_and_refuses_what_it_does_not() {
        let r =
            Rules::parse(r#"{"agents":["w_tools/agents/剪辑"],"localData":{"素材":[]}}"#).unwrap();
        assert_eq!(r.agents, Agents::Only(vec!["w_tools/agents/剪辑".into()]));
        assert_eq!(
            r.view(),
            json!({"agents":["w_tools/agents/剪辑"],"localData":{"素材":[]}})
        );
        assert_eq!(
            Rules::parse(r#"{"agents":"own"}"#).unwrap(),
            Rules::default()
        );
        assert!(Rules::parse(r#"{"agents":"everyone"}"#).is_err());
        assert!(Rules::parse(r#"{"agents":"own","visitors":true}"#).is_err());
        assert!(Rules::parse(r#"{"agents":[""]}"#).is_err());
        // config.json keeps `local_data`.
        let saved = serde_json::to_value(&r).unwrap();
        assert_eq!(
            saved,
            json!({"agents":["w_tools/agents/剪辑"],"local_data":{"素材":[]}})
        );
        assert_eq!(serde_json::from_value::<Rules>(saved).unwrap(), r);
    }

    #[test]
    fn admits_listed_recipes_at_any_version_and_closes_local_data() {
        let r = Rules::parse(
            r#"{"agents":["w_tools/agents/剪辑"],"localData":{"照片库":["w_tools/agents/照片"]}}"#,
        )
        .unwrap();
        assert_eq!(r.refuses(Some("w_tools/agents/剪辑@5")), None);
        assert!(
            r.refuses(Some("w_tools/agents/翻译@1"))
                .unwrap()
                .contains("不包括 w_tools/agents/翻译@1")
        );
        assert!(r.refuses(None).is_some());
        assert!(Rules::default().refuses(None).is_none());
        assert!(r.data_open("素材", Some("w_tools/agents/剪辑")));
        assert!(r.data_open("照片库", Some("w_tools/agents/照片@2")));
        assert!(!r.data_open(" 照片库 ", Some("w_tools/agents/剪辑")));
        assert!(!r.data_open("照片库", None));
    }
}
