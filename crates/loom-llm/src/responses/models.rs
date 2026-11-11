use serde_json::Value;

use crate::types::{ModelCapabilities, ModelInfo, ReasoningEffort};

pub(crate) fn listed_models(models: &[Value]) -> Vec<ModelInfo> {
    let mut listed: Vec<(i64, ModelInfo)> = models
        .iter()
        .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
        .filter(|m| m.get("supported_in_api").and_then(Value::as_bool) != Some(false))
        .filter_map(|m| {
            let slug = m.get("slug").and_then(Value::as_str)?;
            let name = m
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(slug);
            let priority = m
                .get("priority")
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX);
            Some((
                priority,
                ModelInfo {
                    id: slug.to_owned(),
                    display_name: name.to_owned(),
                    capabilities: capabilities(m),
                },
            ))
        })
        .collect();
    listed.sort_by_key(|(priority, _)| *priority);
    listed.into_iter().map(|(_, model)| model).collect()
}

/// The context window and reasoning efforts a Codex model lists. Efforts
/// Loom has no name for (such as `max`) are left out.
fn capabilities(model: &Value) -> ModelCapabilities {
    let efforts = model
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .map(|levels| {
            let mut efforts: Vec<ReasoningEffort> = levels
                .iter()
                .filter_map(|level| level.get("effort")?.as_str())
                .filter_map(|name| serde_json::from_value(Value::from(name)).ok())
                .collect();
            efforts.sort();
            efforts.dedup();
            efforts
        })
        .filter(|efforts| !efforts.is_empty());
    ModelCapabilities {
        context_window: model
            .get("context_window")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0),
        efforts,
        ..ModelCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_listed_models_in_priority_order() {
        let models = json!([
            {"slug": "b", "display_name": "B", "visibility": "list", "priority": 2},
            {"slug": "hidden", "visibility": "hide", "priority": 0},
            {"slug": "a", "visibility": "list", "priority": 1},
            {"slug": "web", "visibility": "list", "supported_in_api": false, "priority": 3}
        ]);
        let listed = listed_models(models.as_array().unwrap());
        let ids: Vec<_> = listed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(listed[0].display_name, "a");
        assert_eq!(listed[1].display_name, "B");
        assert_eq!(listed[0].capabilities, ModelCapabilities::default());
    }

    #[test]
    fn reads_the_context_window_and_efforts() {
        // Trimmed from a real gpt-6.1-sol entry.
        let models = json!([{
            "slug": "gpt-6.1-sol", "visibility": "list", "priority": 1,
            "context_window": 500000, "max_context_window": 872000,
            "supported_reasoning_levels": [
                {"effort": "xhigh"}, {"effort": "low"}, {"effort": "medium"},
                {"effort": "high"}, {"effort": "max"}, {"effort": "ultra"}
            ]
        }]);
        let caps = &listed_models(models.as_array().unwrap())[0].capabilities;
        assert_eq!(caps.context_window, Some(500_000));
        assert_eq!(caps.max_output_tokens, None);
        assert_eq!(
            caps.efforts.as_deref(),
            Some(
                &[
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::Xhigh
                ][..]
            )
        );
        assert_eq!(
            caps.nearest_effort(ReasoningEffort::Minimal),
            Some(ReasoningEffort::Low)
        );
    }
}
