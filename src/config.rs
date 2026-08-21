use crate::Result;
use crate::model::{ResumeProfile, RoleProfile, SearchConfig, object, string, strings};
use crate::text::{dedupe, normalize};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::path::Path;

pub const STATE_DB: &str = "jobbot_state.sqlite3";
pub const MIN_MATCH_SCORE: i64 = 28;
pub const BORDERLINE_MARGIN: i64 = 6;

pub fn load_dotenv(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(());
    }
    for raw in fs::read_to_string(path)?.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !key.is_empty() && env::var_os(key).is_none() {
            let value = value.trim().trim_matches(['\'', '"']);
            // SAFETY: RoleForge loads the environment before starting worker threads.
            unsafe { env::set_var(key, value) };
        }
    }
    Ok(())
}

fn read_json(path: impl AsRef<Path>) -> Result<Value> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

pub fn load_resume(path: impl AsRef<Path>) -> Result<ResumeProfile> {
    let raw = read_json(path)?;
    let root = object(&raw);
    let personal = root
        .get("personal_info")
        .map(object)
        .unwrap_or_else(|| object(&Value::Null));
    let location = personal
        .get("location")
        .map(object)
        .unwrap_or_else(|| object(&Value::Null));
    let prefs = personal
        .get("preferences")
        .map(object)
        .unwrap_or_else(|| object(&Value::Null));
    let technical = root
        .get("technical_skills")
        .map(object)
        .unwrap_or_else(|| object(&Value::Null));

    let preferred_locations = dedupe(
        strings(prefs.get("preferred_locations"))
            .into_iter()
            .chain([
                string(location.get("city")),
                string(location.get("country")),
            ])
            .map(|item| normalize(&item)),
    );
    let mut evidence = vec![];
    if let Some(summary) = root.get("summary").and_then(Value::as_str) {
        evidence.push(("Summary".into(), summary.into()));
    }
    if let Some(items) = root.get("experience").and_then(Value::as_array) {
        for item in items {
            let entry = object(item);
            let label = [string(entry.get("title")), string(entry.get("company"))]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" at ");
            let detail = entry
                .get("responsibilities")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            if !detail.is_empty() {
                evidence.push((label, detail));
            }
        }
    }

    Ok(ResumeProfile {
        candidate_name: string(personal.get("name")),
        candidate_title: string(personal.get("title")),
        resume_summary: string(root.get("summary")),
        remote: prefs
            .get("remote")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        hybrid: prefs
            .get("hybrid")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        onsite: prefs
            .get("onsite")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        relocation: prefs
            .get("relocation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        minimum_salary_gbp: prefs
            .get("minimum_salary_gbp")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        preferred_locations,
        target_roles: strings(personal.get("target_roles")),
        skills: strings(technical.get("skills")),
        competencies: strings(technical.get("competencies")),
        evidence,
        raw,
    })
}

fn default_role_profiles() -> Vec<RoleProfile> {
    vec![
        RoleProfile {
            name: "core_it_support".into(),
            display_name: "Core IT Support".into(),
            title_keywords: words(&[
                "it support engineer",
                "it support analyst",
                "it technician",
                "service desk",
                "help desk",
                "helpdesk",
                "desktop support",
                "technical support",
            ]),
            description_keywords: words(&[
                "user support",
                "ticket management",
                "vip support",
                "hardware support",
                "onboarding",
                "offboarding",
                "troubleshooting",
            ]),
            title_boost: 18,
            description_boost: 5,
            priority: 100,
        },
        RoleProfile {
            name: "systems_administration".into(),
            display_name: "Systems Administration".into(),
            title_keywords: words(&[
                "systems administrator",
                "system administrator",
                "sysadmin",
                "endpoint engineer",
                "microsoft 365 administrator",
                "identity administrator",
            ]),
            description_keywords: words(&[
                "active directory",
                "azure ad",
                "microsoft entra",
                "windows",
                "linux",
                "sharepoint",
                "exchange",
                "identity and access management",
            ]),
            title_boost: 16,
            description_boost: 4,
            priority: 90,
        },
        RoleProfile {
            name: "support_infrastructure".into(),
            display_name: "Support Infrastructure".into(),
            title_keywords: words(&[
                "support engineer",
                "support analyst",
                "infrastructure engineer",
                "operations analyst",
                "technical operations",
            ]),
            description_keywords: words(&[
                "microsoft 365",
                "network",
                "access control",
                "hardware upgrades",
                "vendor coordination",
                "documentation",
            ]),
            title_boost: 9,
            description_boost: 3,
            priority: 70,
        },
    ]
}

fn words(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

pub fn load_search_config(path: impl AsRef<Path>) -> Result<SearchConfig> {
    let path = path.as_ref();
    let raw = if path.exists() {
        read_json(path)?
    } else {
        json!({})
    };
    let root = object(&raw);
    let role_profiles = root
        .get("role_profiles")
        .and_then(Value::as_array)
        .and_then(|items| {
            let parsed: Vec<RoleProfile> = items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect();
            (!parsed.is_empty()).then_some(parsed)
        })
        .unwrap_or_else(default_role_profiles);
    let mut config = SearchConfig {
        company_whitelist: strings(root.get("company_whitelist")),
        company_blacklist: strings(root.get("company_blacklist")),
        priority_companies: strings(root.get("priority_companies")),
        role_profiles,
        ..SearchConfig::default()
    };
    if root.get("daily_digest").is_some_and(Value::is_object) {
        config.daily_digest = root["daily_digest"].clone();
    }
    if root.get("feedback").is_some_and(Value::is_object) {
        config.feedback = root["feedback"].clone();
    }
    Ok(config)
}

pub fn built_in_sources() -> Vec<Value> {
    vec![
        json!({"name":"efc_technology_uk","url":"https://www.efinancialcareers.com/jobs/technology/in-united-kingdom","type":"efc_html","context_terms":"technology finance fintech banking trading london united kingdom hybrid in-office","min_interval_seconds":1800}),
        json!({"name":"wwr_all","url":"https://weworkremotely.com/remote-jobs.rss","min_interval_seconds":900}),
        json!({"name":"remotive_software_dev","type":"remotive","category":"software-dev","display_name":"Remotive","min_interval_seconds":3600}),
        json!({"name":"remotive_devops","type":"remotive","category":"devops-sysadmin","display_name":"Remotive","min_interval_seconds":3600}),
        json!({"name":"remotefirstjobs_all","url":"https://remotefirstjobs.com/rss/jobs.rss","min_interval_seconds":3600}),
        json!({"name":"python_org_jobs","url":"https://www.python.org/jobs/feed/rss/","context_terms":"london uk united kingdom hybrid office","min_interval_seconds":3600}),
        json!({"name":"jobs_ac_uk_tech_london","url":"https://www.jobs.ac.uk/jobs/london?format=rss","context_terms":"tech support developer engineer hybrid in-office","min_interval_seconds":3600}),
        json!({"name":"google_alerts_corporate","url":"https://www.google.com/alerts/feeds/09910730576829865385/10714127524543454759","min_interval_seconds":3600}),
        json!({"name":"google_alerts_core_it_support","url":"https://www.google.com/alerts/feeds/09910730576829865385/13401058173831861136","min_interval_seconds":3600}),
        json!({"name":"google_alerts_infrastructure","url":"https://www.google.com/alerts/feeds/09910730576829865385/15611871513157400765","min_interval_seconds":3600}),
        json!({"name":"google_alerts_dev_adjacent","url":"https://www.google.com/alerts/feeds/09910730576829865385/17528292640163305738","min_interval_seconds":3600}),
        json!({"name":"jobicy_fintech_uk","url":"https://jobicy.com/feed/job_feed?job_categories=engineering,technical-support,accounting-finance&search_keywords=fintech&search_region=uk","min_interval_seconds":3600}),
        json!({"name":"jobicy_payments_uk","url":"https://jobicy.com/feed/job_feed?job_categories=engineering,technical-support,accounting-finance&search_keywords=payments&search_region=uk","min_interval_seconds":3600}),
        json!({"name":"adzuna_it_london","type":"adzuna","country":"gb","what":"IT support systems administrator service desk","where":"London","display_name":"Adzuna","min_interval_seconds":3600}),
        json!({"name":"reed_it_london","type":"reed","keywords":"IT support systems administrator service desk","location":"London","display_name":"Reed","min_interval_seconds":3600}),
        json!({"name":"jooble_it_london","type":"jooble","keywords":"IT support systems administrator service desk","location":"London","display_name":"Jooble","min_interval_seconds":3600}),
        json!({"name":"themuse_it_data","type":"themuse","category":"IT & Data","display_name":"The Muse","min_interval_seconds":3600}),
        json!({"name":"arbeitnow_all","type":"arbeitnow","display_name":"Arbeitnow","min_interval_seconds":3600}),
    ]
}

pub fn load_company_boards(path: impl AsRef<Path>) -> Result<Vec<Value>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(vec![]);
    }
    let value = read_json(path)?;
    Ok(value
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|board| board.is_object())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_operational() {
        assert_eq!(built_in_sources().len(), 18);
        assert_eq!(default_role_profiles().len(), 3);
    }

    #[test]
    fn example_resume_is_valid() {
        let profile = load_resume("resume.example.json").unwrap();
        assert_eq!(profile.candidate_name, "Alex Example");
        assert!(!profile.target_roles.is_empty());
    }
}
