use serde_json::{Map, Value};

use crate::error::ConfigError;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigDocument(Map<String, Value>);

impl From<Map<String, Value>> for ConfigDocument {
    fn from(map: Map<String, Value>) -> Self {
        Self(map)
    }
}

impl ConfigDocument {
    pub fn parse(text: &str, origin: &str) -> Result<Self, ConfigError> {
        let parse_error = |message: String| ConfigError::Parse {
            origin: origin.to_owned(),
            message,
        };
        let document: Value =
            serde_saphyr::from_str(text).map_err(|err| parse_error(err.to_string()))?;
        match document {
            Value::Null => Ok(Self::default()),
            Value::Object(map) => Ok(Self(map)),
            _ => Err(parse_error("the file must be a mapping of keys".to_owned())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, path: &str) -> Option<&Value> {
        let mut parts = path.split('.');
        let mut value = self.0.get(parts.next()?)?;
        for part in parts {
            value = value.as_object()?.get(part)?;
        }
        Some(value)
    }

    pub fn set(&mut self, path: &str, value: Value) {
        let parts: Vec<&str> = path.split('.').collect();
        let Some((last, parents)) = parts.split_last() else {
            return;
        };
        let mut map = &mut self.0;
        for part in parents {
            let entry = map
                .entry(part.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            let Value::Object(next) = entry else {
                return;
            };
            map = next;
        }
        map.insert(last.to_string(), value);
    }

    pub fn remove(&mut self, path: &str) -> bool {
        fn remove_key(map: &mut Map<String, Value>, key: &str) -> bool {
            if !map.contains_key(key) {
                return false;
            }
            *map = std::mem::take(map)
                .into_iter()
                .filter(|(k, _)| k != key)
                .collect();
            true
        }
        fn remove_in(map: &mut Map<String, Value>, parts: &[&str]) -> bool {
            match parts {
                [] => false,
                [last] => remove_key(map, last),
                [first, rest @ ..] => {
                    let Some(Value::Object(child)) = map.get_mut(*first) else {
                        return false;
                    };
                    let removed = remove_in(child, rest);
                    if removed && child.is_empty() {
                        remove_key(map, first);
                    }
                    removed
                }
            }
        }
        let parts: Vec<&str> = path.split('.').collect();
        remove_in(&mut self.0, &parts)
    }

    pub fn to_yaml(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        serde_saphyr::to_string(&self.0).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn values_change_by_path() {
        let mut doc = ConfigDocument::parse(
            "llm:\n  default_model: a/b\ntools:\n  read:\n    max_lines: 10\n",
            "agent.yaml",
        )
        .unwrap();
        assert_eq!(doc.get("llm.default_model"), Some(&json!("a/b")));
        assert_eq!(doc.get("tools.read.max_lines"), Some(&json!(10)));
        assert_eq!(doc.get("tools.shell.pty"), None);
        assert_eq!(doc.get("llm.default_model.x"), None);

        doc.set("tools.shell.pty", json!(true));
        assert_eq!(doc.get("tools.shell.pty"), Some(&json!(true)));
        assert!(doc.remove("tools.read.max_lines"));
        assert!(!doc.remove("tools.read.max_lines"));
        assert_eq!(doc.get("tools.read"), None, "empty parents go away");
        assert!(doc.remove("tools.shell.pty"));
        assert!(doc.remove("llm.default_model"));
        assert!(doc.is_empty());
        assert_eq!(doc.to_yaml(), "");
    }

    #[test]
    fn the_yaml_keeps_the_types() {
        let mut doc = ConfigDocument::parse(
            "policy:\n  default: ask\nenvironment:\n  set:\n    RUST_BACKTRACE: \"1\"\n",
            "agent.yaml",
        )
        .unwrap();
        doc.set("context.compaction_threshold", json!(0.5));
        doc.set("policy", json!("x"));
        doc.set("policy.default", json!("allow"));
        let yaml = doc.to_yaml();
        let again = ConfigDocument::parse(&yaml, "agent.yaml").unwrap();
        assert_eq!(again, doc, "{yaml}");
        assert_eq!(
            again.get("environment.set.RUST_BACKTRACE"),
            Some(&json!("1"))
        );
    }

    #[test]
    fn only_a_mapping_is_a_document() {
        assert!(ConfigDocument::parse("", "agent.yaml").unwrap().is_empty());
        assert!(
            ConfigDocument::parse("# nothing\n", "agent.yaml")
                .unwrap()
                .is_empty()
        );
        assert!(ConfigDocument::parse("- a\n", "agent.yaml").is_err());
        assert!(ConfigDocument::parse("a: [", "agent.yaml").is_err());
    }
}
