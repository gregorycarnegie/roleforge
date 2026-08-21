use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct JobLead {
    pub title: String,
    pub link: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub company: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub salary: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub employment_type: String,
    #[serde(default)]
    pub date_posted: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RoleProfile {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub title_keywords: Vec<String>,
    #[serde(default)]
    pub description_keywords: Vec<String>,
    #[serde(default)]
    pub title_boost: i64,
    #[serde(default)]
    pub description_boost: i64,
    #[serde(default)]
    pub priority: i64,
}

#[derive(Clone, Debug, Default)]
pub struct ResumeProfile {
    pub raw: Value,
    pub candidate_name: String,
    pub candidate_title: String,
    pub resume_summary: String,
    pub remote: bool,
    pub hybrid: bool,
    pub onsite: bool,
    pub relocation: bool,
    pub minimum_salary_gbp: i64,
    pub preferred_locations: Vec<String>,
    pub target_roles: Vec<String>,
    pub skills: Vec<String>,
    pub competencies: Vec<String>,
    pub evidence: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct SearchConfig {
    pub company_whitelist: Vec<String>,
    pub company_blacklist: Vec<String>,
    pub priority_companies: Vec<String>,
    pub role_profiles: Vec<RoleProfile>,
    pub daily_digest: Value,
    pub feedback: Value,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            company_whitelist: vec![],
            company_blacklist: vec![],
            priority_companies: vec![],
            role_profiles: vec![],
            daily_digest: serde_json::json!({
                "enabled": true, "hour_utc": 7, "max_items": 8, "page_size": 4,
                "statuses": ["new", "reviewed", "applied", "interview"]
            }),
            feedback: serde_json::json!({"enabled": true, "min_samples": 2}),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MatchResult {
    pub qualified: bool,
    pub score: i64,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_role_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<Value>,
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub matched: Option<Value>,
}

pub fn string(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

pub fn object(value: &Value) -> &Map<String, Value> {
    value.as_object().unwrap_or_else(|| {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        EMPTY.get_or_init(Map::new)
    })
}

pub fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_accessors_tolerate_wrong_shapes() {
        let value =
            json!({"name": "  Acme  ", "count": 3, "tags": ["a", "  ", " b "], "empty": []});
        assert_eq!(string(value.get("name")), "Acme");
        assert_eq!(string(value.get("count")), "");
        assert_eq!(string(None), "");

        assert_eq!(object(&value).len(), 4);
        assert!(object(&json!("scalar")).is_empty());

        assert_eq!(strings(value.get("tags")), ["a", "b"]);
        assert!(strings(value.get("empty")).is_empty());
        assert!(strings(value.get("name")).is_empty());
        assert!(strings(None).is_empty());
    }
}
