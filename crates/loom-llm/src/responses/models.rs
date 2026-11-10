use serde_json::Value;

use crate::types::ModelInfo;

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
                },
            ))
        })
        .collect();
    listed.sort_by_key(|(priority, _)| *priority);
    listed.into_iter().map(|(_, model)| model).collect()
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
    }
}
