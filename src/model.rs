use serde::Deserialize;
use serde_json::{Map, Value};
use std::sync::LazyLock;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobLead {
    pub title: String,
    pub link: String,
    pub source: String,
    pub company: String,
    pub location: String,
    pub salary: String,
    pub description: String,
    pub employment_type: String,
    pub date_posted: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
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

#[derive(Clone, Debug, Default)]
pub struct SearchConfig {
    pub company_whitelist: Vec<String>,
    pub company_blacklist: Vec<String>,
    pub priority_companies: Vec<String>,
    pub role_profiles: Vec<RoleProfile>,
    /// Absent keys fall back in the readers themselves, so the unset default is
    /// Null rather than a second copy of every digest default.
    pub daily_digest: Value,
}

#[derive(Clone, Debug)]
pub struct MatchResult {
    pub qualified: bool,
    pub score: i64,
    pub candidate: Option<Value>,
}

pub fn string(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

pub fn object(value: &Value) -> &Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    value.as_object().unwrap_or(&EMPTY)
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
    use rstest::rstest;
    use serde_json::json;

    #[rstest]
    #[case::trimmed(Some(json!("  Acme  ")), "Acme")]
    #[case::number(Some(json!(3)), "")]
    #[case::missing(None, "")]
    fn string_tolerates_wrong_shapes(#[case] value: Option<Value>, #[case] expected: &str) {
        assert_eq!(string(value.as_ref()), expected);
    }

    #[rstest]
    #[case::object(json!({"name": "Acme", "count": 3, "tags": ["a"], "empty": []}), 4)]
    #[case::scalar(json!("scalar"), 0)]
    fn object_tolerates_wrong_shapes(#[case] value: Value, #[case] expected_len: usize) {
        assert_eq!(object(&value).len(), expected_len);
    }

    #[rstest]
    #[case::trimmed(Some(json!(["a", "  ", " b "])), &["a", "b"])]
    #[case::empty(Some(json!([])), &[])]
    #[case::scalar(Some(json!("  Acme  ")), &[])]
    #[case::missing(None, &[])]
    fn strings_tolerates_wrong_shapes(#[case] value: Option<Value>, #[case] expected: &[&str]) {
        assert_eq!(strings(value.as_ref()), expected);
    }
}
