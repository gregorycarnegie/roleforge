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

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn defaults_are_operational() {
        assert_eq!(built_in_sources().len(), 18);
        assert_eq!(default_role_profiles().len(), 3);
        assert!(
            built_in_sources()
                .iter()
                .all(|source| source.get("name").is_some_and(Value::is_string))
        );
        assert!(
            default_role_profiles()
                .iter()
                .all(|role| !role.title_keywords.is_empty() && role.title_boost > 0)
        );
    }

    #[test]
    fn words_maps_every_entry() {
        assert_eq!(words(&["one", "two"]), ["one", "two"]);
        assert!(words(&[]).is_empty());
    }

    #[test]
    fn example_resume_is_valid() {
        let profile = load_resume("resume.example.json").unwrap();
        assert_eq!(profile.candidate_name, "Alex Example");
        assert!(!profile.target_roles.is_empty());
    }

    #[test]
    fn load_resume_reads_every_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "resume.json",
            r#"{
                "summary": "Support specialist",
                "personal_info": {
                    "name": "Alex", "title": "Engineer",
                    "target_roles": ["it support engineer", "  ", "service desk"],
                    "location": {"city": "London", "country": "UK"},
                    "preferences": {
                        "remote": true, "hybrid": true, "onsite": false, "relocation": true,
                        "minimum_salary_gbp": 42000,
                        "preferred_locations": ["Greater London"]
                    }
                },
                "technical_skills": {"skills": ["Intune"], "competencies": ["Coaching"]},
                "experience": [
                    {"title": "Engineer", "company": "Acme",
                     "responsibilities": ["Ran the estate", "Fixed things"]},
                    {"title": "Intern", "company": "Globex"}
                ]
            }"#,
        );
        let profile = load_resume(&path).unwrap();
        assert_eq!(profile.candidate_name, "Alex");
        assert_eq!(profile.candidate_title, "Engineer");
        assert_eq!(profile.resume_summary, "Support specialist");
        assert!(profile.remote && profile.hybrid && profile.relocation);
        assert!(!profile.onsite);
        assert_eq!(profile.minimum_salary_gbp, 42_000);
        // Preferences, then city and country, normalized and deduplicated.
        assert_eq!(
            profile.preferred_locations,
            ["greater london", "london", "uk"]
        );
        assert_eq!(
            profile.target_roles,
            ["it support engineer", "service desk"]
        );
        assert_eq!(profile.skills, ["Intune"]);
        assert_eq!(profile.competencies, ["Coaching"]);
        // The summary, then only experience entries with responsibilities.
        assert_eq!(
            profile.evidence,
            [
                ("Summary".to_owned(), "Support specialist".to_owned()),
                (
                    "Engineer at Acme".to_owned(),
                    "Ran the estate Fixed things".to_owned()
                )
            ]
        );
        assert_eq!(profile.raw["personal_info"]["name"], "Alex");
    }

    #[test]
    fn load_resume_tolerates_an_empty_document() {
        let dir = tempfile::tempdir().unwrap();
        let profile = load_resume(write(&dir, "bare.json", "{}")).unwrap();
        assert_eq!(profile.candidate_name, "");
        assert_eq!(profile.minimum_salary_gbp, 0);
        assert!(!profile.remote && !profile.hybrid && !profile.onsite && !profile.relocation);
        assert!(profile.preferred_locations.is_empty());
        assert!(profile.evidence.is_empty());

        assert!(load_resume(write(&dir, "bad.json", "not json")).is_err());
        assert!(load_resume(dir.path().join("missing.json")).is_err());
    }

    #[test]
    fn search_config_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let missing = load_search_config(dir.path().join("missing.json")).unwrap();
        assert!(missing.company_whitelist.is_empty());
        assert_eq!(missing.role_profiles.len(), 3);
        assert_eq!(missing.daily_digest["hour_utc"], 7);
        assert_eq!(missing.feedback["min_samples"], 2);

        // Present but empty: still defaults.
        let empty = load_search_config(write(&dir, "empty.json", "{}")).unwrap();
        assert_eq!(empty.role_profiles.len(), 3);

        // A role_profiles array that parses to nothing also falls back.
        let junk = load_search_config(write(
            &dir,
            "junk.json",
            r#"{"role_profiles": [{"no_name_field": 1}]}"#,
        ))
        .unwrap();
        assert_eq!(junk.role_profiles.len(), 3);
    }

    #[test]
    fn search_config_reads_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "search.json",
            r#"{
                "company_whitelist": ["Acme", "  "],
                "company_blacklist": ["SpamCorp"],
                "priority_companies": ["Globex"],
                "role_profiles": [{"name": "custom", "title_keywords": ["support"],
                                   "title_boost": 5, "priority": 1}],
                "daily_digest": {"hour_utc": 19},
                "feedback": {"enabled": false}
            }"#,
        );
        let config = load_search_config(&path).unwrap();
        assert_eq!(config.company_whitelist, ["Acme"]);
        assert_eq!(config.company_blacklist, ["SpamCorp"]);
        assert_eq!(config.priority_companies, ["Globex"]);
        assert_eq!(config.role_profiles.len(), 1);
        assert_eq!(config.role_profiles[0].name, "custom");
        assert_eq!(config.daily_digest["hour_utc"], 19);
        assert_eq!(config.feedback["enabled"], false);

        // Non-object daily_digest/feedback keep the defaults.
        let scalar = load_search_config(write(
            &dir,
            "scalar.json",
            r#"{"daily_digest": 5, "feedback": "on"}"#,
        ))
        .unwrap();
        assert_eq!(scalar.daily_digest["hour_utc"], 7);
        assert_eq!(scalar.feedback["min_samples"], 2);

        assert!(load_search_config(write(&dir, "bad.json", "{")).is_err());
    }

    #[test]
    fn company_boards_are_filtered_to_objects() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_company_boards(dir.path().join("missing.json"))
                .unwrap()
                .is_empty()
        );
        let boards = load_company_boards(write(
            &dir,
            "boards.json",
            r#"[{"name": "acme"}, "skip me", 5, {"name": "globex"}]"#,
        ))
        .unwrap();
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0]["name"], "acme");
        assert_eq!(boards[1]["name"], "globex");

        // A document that is not an array yields nothing.
        assert!(
            load_company_boards(write(&dir, "object.json", r#"{"name": "acme"}"#))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn dotenv_sets_only_unset_keys() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test, no worker threads are running.
        unsafe {
            env::set_var("ROLEFORGE_DOTENV_EXISTING", "original");
            env::remove_var("ROLEFORGE_DOTENV_NEW");
            env::remove_var("ROLEFORGE_DOTENV_QUOTED");
            env::remove_var("ROLEFORGE_DOTENV_COMMENT");
        }
        let path = write(
            &dir,
            ".env",
            "\n  # a comment\n\
             ROLEFORGE_DOTENV_NEW=fresh\n\
             ROLEFORGE_DOTENV_EXISTING=ignored\n\
             ROLEFORGE_DOTENV_QUOTED = \"quoted value\" \n\
             #ROLEFORGE_DOTENV_COMMENT=nope\n\
             no equals sign here\n\
             =blank key\n",
        );
        load_dotenv(&path).unwrap();
        assert_eq!(env::var("ROLEFORGE_DOTENV_NEW").unwrap(), "fresh");
        assert_eq!(env::var("ROLEFORGE_DOTENV_EXISTING").unwrap(), "original");
        assert_eq!(env::var("ROLEFORGE_DOTENV_QUOTED").unwrap(), "quoted value");
        assert!(env::var("ROLEFORGE_DOTENV_COMMENT").is_err());
        assert!(env::var("#ROLEFORGE_DOTENV_COMMENT").is_err());

        // A missing file is not an error.
        load_dotenv(dir.path().join("missing.env")).unwrap();
    }
}
