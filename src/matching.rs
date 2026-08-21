use crate::config::MIN_MATCH_SCORE;
use crate::model::{JobLead, MatchResult, ResumeProfile, RoleProfile, SearchConfig};
use crate::text::{
    clean, contains_phrase, dedupe, find_matches, normalize, split_title_company, truncate,
};
use regex::Regex;
use serde_json::json;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq)]
pub struct SalaryInfo {
    pub min_gbp: i64,
    pub max_gbp: i64,
    pub currency: String,
    pub cadence: String,
}

pub fn location_fit(
    text: &str,
    profile: &ResumeProfile,
    lockouts: &[String],
) -> (bool, &'static str) {
    let context: String = normalize(text).chars().take(1000).collect();
    let remote = ["remote", "anywhere", "wfh"]
        .iter()
        .any(|term| contains_phrase(&context, term));
    let hybrid = context.contains("hybrid");
    let onsite = [
        "in office",
        "in-office",
        "onsite",
        "on site",
        "on-site",
        "office based",
        "office-based",
    ]
    .iter()
    .any(|term| context.contains(term));
    let local = profile
        .preferred_locations
        .iter()
        .any(|term| context.contains(term));
    let locked = lockouts.iter().any(|term| context.contains(term));
    if profile.relocation {
        (true, "location fit: relocation allowed")
    } else if profile.remote && remote && !locked {
        (true, "location fit: remote compatible")
    } else if profile.hybrid && local && (hybrid || !remote) {
        (true, "location fit: local/hybrid compatible")
    } else if profile.onsite && local && (onsite || !remote) {
        (true, "location fit: local/onsite compatible")
    } else if locked {
        (false, "location reject: explicit geographic lockout")
    } else {
        (false, "location reject: not aligned with preferences")
    }
}

fn currency(token: &str) -> Option<(&'static str, f64)> {
    match token.trim().to_lowercase().as_str() {
        "£" | "gbp" | "pound" | "pounds" => Some(("gbp", 1.0)),
        "$" | "us$" | "usd" | "dollar" | "dollars" => Some(("usd", 0.79)),
        "€" | "eur" | "euro" | "euros" => Some(("eur", 0.86)),
        _ => None,
    }
}

fn cadence(context: &str) -> (&'static str, f64) {
    let text = normalize(context);
    if ["per hour", "a hour", "/hour", "hourly"]
        .iter()
        .any(|term| text.contains(term))
    {
        ("hour", 2080.0)
    } else if ["per day", "a day", "/day", "daily", "day rate", "day-rate"]
        .iter()
        .any(|term| text.contains(term))
    {
        ("day", 260.0)
    } else if ["per month", "a month", "/month", "monthly"]
        .iter()
        .any(|term| text.contains(term))
    {
        ("month", 12.0)
    } else {
        ("year", 1.0)
    }
}

fn amount(value: &str, k: Option<&str>) -> Option<f64> {
    value
        .replace(',', "")
        .parse::<f64>()
        .ok()
        .map(|value| if k.is_some() { value * 1000.0 } else { value })
}

pub fn extract_salary(text: &str) -> Option<SalaryInfo> {
    static RANGE_PREFIX: OnceLock<Regex> = OnceLock::new();
    static RANGE_SUFFIX: OnceLock<Regex> = OnceLock::new();
    static SINGLE_PREFIX: OnceLock<Regex> = OnceLock::new();
    static SINGLE_SUFFIX: OnceLock<Regex> = OnceLock::new();
    let patterns = [
        (RANGE_PREFIX.get_or_init(|| Regex::new(r"(?i)(£|gbp|us\$|usd|\$|eur|€)\s*(\d+(?:\.\d+)?)\s*(k)?\s*(?:-|–|—|to)\s*(?:£|gbp|us\$|usd|\$|eur|€)?\s*(\d+(?:\.\d+)?)\s*(k)?").unwrap()), true, true),
        (RANGE_SUFFIX.get_or_init(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(k)?\s*(?:-|–|—|to)\s*(\d+(?:\.\d+)?)\s*(k)?\s*(gbp|usd|eur|pounds?|dollars?|euros?)").unwrap()), true, false),
        (SINGLE_PREFIX.get_or_init(|| Regex::new(r"(?i)(£|gbp|us\$|usd|\$|eur|€)\s*(\d+(?:\.\d+)?)\s*(k)?").unwrap()), false, true),
        (SINGLE_SUFFIX.get_or_init(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(k)?\s*(gbp|usd|eur|pounds?|dollars?|euros?)").unwrap()), false, false),
    ];
    let normalized = clean(text).to_lowercase();
    for (regex, range, prefix) in patterns {
        let Some(caps) = regex.captures(&normalized) else {
            continue;
        };
        let (currency_text, low, high) = match (range, prefix) {
            (true, true) => (
                caps.get(1)?.as_str(),
                amount(caps.get(2)?.as_str(), caps.get(3).map(|m| m.as_str()))?,
                amount(caps.get(4)?.as_str(), caps.get(5).map(|m| m.as_str()))?,
            ),
            (true, false) => (
                caps.get(5)?.as_str(),
                amount(caps.get(1)?.as_str(), caps.get(2).map(|m| m.as_str()))?,
                amount(caps.get(3)?.as_str(), caps.get(4).map(|m| m.as_str()))?,
            ),
            (false, true) => {
                let value = amount(caps.get(2)?.as_str(), caps.get(3).map(|m| m.as_str()))?;
                (caps.get(1)?.as_str(), value, value)
            }
            (false, false) => {
                let value = amount(caps.get(1)?.as_str(), caps.get(2).map(|m| m.as_str()))?;
                (caps.get(3)?.as_str(), value, value)
            }
        };
        let (currency, rate) = currency(currency_text)?;
        let matched = caps.get(0)?;
        let end = (matched.end() + 32).min(normalized.len());
        let (cadence, multiplier) = cadence(&normalized[matched.start()..end]);
        if cadence == "year" && high < 1000.0 {
            continue;
        }
        return Some(SalaryInfo {
            min_gbp: (low.min(high) * rate * multiplier).round() as i64,
            max_gbp: (low.max(high) * rate * multiplier).round() as i64,
            currency: currency.into(),
            cadence: cadence.into(),
        });
    }
    None
}

fn push_reason(reasons: &mut Vec<String>, reason: impl Into<String>) {
    let reason = reason.into();
    if !reason.is_empty() && !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn company_preference(
    company: &str,
    config: &SearchConfig,
) -> (bool, i64, &'static str, bool, String) {
    let company = normalize(company);
    if let Some(item) = config
        .company_blacklist
        .iter()
        .find(|item| contains_phrase(&company, item))
    {
        return (
            false,
            0,
            "blacklist",
            false,
            format!("company reject: blacklisted employer {item}"),
        );
    }
    if let Some(item) = config
        .priority_companies
        .iter()
        .find(|item| contains_phrase(&company, item))
    {
        return (
            true,
            14,
            "priority",
            true,
            format!("company shortlist: {item}"),
        );
    }
    if let Some(item) = config
        .company_whitelist
        .iter()
        .find(|item| contains_phrase(&company, item))
    {
        return (
            true,
            8,
            "whitelist",
            false,
            format!("company whitelist: {item}"),
        );
    }
    (true, 0, "none", false, String::new())
}

fn best_role_profile(
    title: &str,
    description: &str,
    profiles: &[RoleProfile],
) -> Option<(RoleProfile, Vec<String>, Vec<String>, i64)> {
    profiles
        .iter()
        .filter_map(|profile| {
            let title_matches = find_matches(title, &profile.title_keywords, 3);
            let description_matches: Vec<_> =
                find_matches(description, &profile.description_keywords, 3)
                    .into_iter()
                    .filter(|item| !title_matches.contains(item))
                    .collect();
            if title_matches.is_empty() && description_matches.is_empty() {
                return None;
            }
            let delta = title_matches.len() as i64 * profile.title_boost
                + description_matches.len() as i64 * profile.description_boost;
            Some((profile.clone(), title_matches, description_matches, delta))
        })
        .max_by_key(|(profile, title, desc, delta)| {
            (*delta, profile.priority, title.len(), desc.len())
        })
}

fn apply_weights(
    title: &str,
    weights: &[(&str, i64)],
    reasons: &mut Vec<String>,
    label: &str,
    sign: i64,
) -> i64 {
    let matched: Vec<_> = weights
        .iter()
        .filter(|(phrase, _)| contains_phrase(title, phrase))
        .collect();
    if !matched.is_empty() {
        push_reason(
            reasons,
            format!(
                "{label}: {}",
                matched
                    .iter()
                    .take(3)
                    .map(|(p, _)| *p)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    sign * matched.iter().map(|(_, weight)| *weight).sum::<i64>()
}

pub fn score_job(
    item: &JobLead,
    source: &str,
    profile: &ResumeProfile,
    config: &SearchConfig,
    timestamp: &str,
    lockouts: &[String],
) -> MatchResult {
    let title = clean(&item.title);
    let description = clean(&item.description);
    let full = format!(
        "{title} {description} {} {}",
        item.location, item.employment_type
    );
    let mut reasons = vec![];
    let (location_ok, location_reason) = location_fit(&full, profile, lockouts);
    push_reason(&mut reasons, location_reason);
    if !location_ok {
        return MatchResult {
            qualified: false,
            score: 0,
            reasons,
            matched_role_profile: None,
            candidate: None,
            matched: None,
        };
    }

    let company = if item.company.trim().is_empty() {
        split_title_company(&title).1.or_else(source)
    } else {
        clean(&item.company)
    };
    let (company_ok, mut score, control, shortlisted, company_reason) =
        company_preference(&company, config);
    push_reason(&mut reasons, company_reason);
    if !company_ok {
        return MatchResult {
            qualified: false,
            score: 0,
            reasons,
            matched_role_profile: None,
            candidate: None,
            matched: None,
        };
    }

    let target_title = find_matches(&title, &profile.target_roles, 3);
    if !target_title.is_empty() {
        score += 48.min(34 + 7 * (target_title.len() as i64 - 1));
        push_reason(
            &mut reasons,
            format!("target role in title: {}", target_title.join(", ")),
        );
    }
    let target_desc: Vec<_> = find_matches(&description, &profile.target_roles, 3)
        .into_iter()
        .filter(|item| !target_title.contains(item))
        .collect();
    if !target_desc.is_empty() {
        score += 18.min(9 * target_desc.len() as i64);
        push_reason(
            &mut reasons,
            format!("target role in description: {}", target_desc.join(", ")),
        );
    }

    let skill_title = find_matches(&title, &profile.skills, 4);
    if !skill_title.is_empty() {
        score += 18.min(6 * skill_title.len() as i64);
        push_reason(
            &mut reasons,
            format!("skills in title: {}", skill_title.join(", ")),
        );
    }
    let skill_desc: Vec<_> = find_matches(&description, &profile.skills, 5)
        .into_iter()
        .filter(|item| !skill_title.contains(item))
        .collect();
    if !skill_desc.is_empty() {
        score += 20.min(4 * skill_desc.len() as i64);
        push_reason(
            &mut reasons,
            format!("skills in description: {}", skill_desc.join(", ")),
        );
    }
    let competencies = find_matches(&full, &profile.competencies, 4);
    if !competencies.is_empty() {
        score += 12.min(3 * competencies.len() as i64);
        push_reason(
            &mut reasons,
            format!("competencies matched: {}", competencies.join(", ")),
        );
    }

    score += apply_weights(&title, POSITIVE_WEIGHTS, &mut reasons, "title boost", 1);
    score += apply_weights(&title, NEGATIVE_WEIGHTS, &mut reasons, "title penalty", -1);
    score += apply_weights(
        &title,
        SENIORITY_WEIGHTS,
        &mut reasons,
        "seniority penalty",
        -1,
    );

    let mut role_name = String::new();
    let mut role_title = vec![];
    let mut role_desc = vec![];
    if let Some((role, title_matches, desc_matches, delta)) =
        best_role_profile(&title, &description, &config.role_profiles)
    {
        score += delta;
        role_name = if role.display_name.is_empty() {
            role.name
        } else {
            role.display_name
        };
        role_title = title_matches;
        role_desc = desc_matches;
        push_reason(
            &mut reasons,
            format!(
                "role profile {role_name}: {}",
                dedupe(role_title.iter().chain(&role_desc).cloned()).join(", ")
            ),
        );
    }

    if let Some(salary) = extract_salary(&format!("{title} {description} {}", item.salary)) {
        if profile.minimum_salary_gbp > 0 && salary.max_gbp < profile.minimum_salary_gbp {
            score -= 18;
            push_reason(&mut reasons, "salary penalty: below preference");
        } else if profile.minimum_salary_gbp > 0 && salary.min_gbp >= profile.minimum_salary_gbp {
            score += 4;
            push_reason(&mut reasons, "salary fit: meets preference");
        }
    }

    let feedback_keywords = dedupe(
        target_title
            .iter()
            .chain(&role_title)
            .chain(&skill_title)
            .chain(&skill_desc)
            .chain(&competencies)
            .chain(&role_desc)
            .cloned(),
    )
    .into_iter()
    .take(8)
    .collect::<Vec<_>>();
    let why = feedback_keywords
        .iter()
        .take(3)
        .map(|keyword| format!("Matches your {keyword} experience."))
        .collect::<Vec<_>>();
    let bullets = profile
        .evidence
        .iter()
        .filter(|(_, detail)| {
            feedback_keywords
                .iter()
                .any(|keyword| contains_phrase(detail, keyword))
        })
        .take(3)
        .map(|(label, detail)| format!("{}: {}", label, truncate(detail, 180)))
        .collect::<Vec<_>>();
    let candidate = json!({
        "time": timestamp, "title": title, "description": truncate(&description, 1500), "link": clean(&item.link), "source": source,
        "company": company, "score": score, "reasons": reasons.iter().take(6).collect::<Vec<_>>(), "shortlisted": shortlisted,
        "company_control": control, "role_profile": role_name, "why_this_fits": why, "resume_bullet_suggestions": bullets,
        "intro_message": format!("Hi, I'm interested in the {title} role at {company}. My background aligns well with the requirements."),
        "application_ready": score >= 45, "feedback_keywords": feedback_keywords
    });
    let qualified = score >= MIN_MATCH_SCORE;
    MatchResult {
        qualified,
        score,
        reasons: reasons.into_iter().take(6).collect(),
        matched_role_profile: (!role_name.is_empty()).then_some(role_name),
        candidate: Some(candidate.clone()),
        matched: qualified.then_some(candidate),
    }
}

trait OrElseString {
    fn or_else(self, fallback: &str) -> String;
}
impl OrElseString for String {
    fn or_else(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.into()
        } else {
            self
        }
    }
}

const POSITIVE_WEIGHTS: &[(&str, i64)] = &[
    ("systems administrator", 32),
    ("system administrator", 32),
    ("it support engineer", 30),
    ("it support analyst", 28),
    ("desktop support", 28),
    ("service desk", 26),
    ("help desk", 24),
    ("helpdesk", 24),
    ("technical support", 22),
    ("it technician", 24),
    ("support engineer", 18),
    ("support analyst", 18),
    ("infrastructure engineer", 18),
    ("endpoint engineer", 16),
    ("microsoft 365", 14),
    ("identity access", 18),
    ("iam", 10),
];
const NEGATIVE_WEIGHTS: &[(&str, i64)] = &[
    ("data engineer", 40),
    ("data scientist", 45),
    ("machine learning", 45),
    ("ml engineer", 45),
    ("ai engineer", 40),
    ("software engineer", 18),
    ("backend engineer", 20),
    ("frontend engineer", 20),
    ("full stack", 18),
    ("product manager", 35),
    ("marketing", 35),
    ("sales", 35),
    ("account executive", 35),
    ("account manager", 22),
];
const SENIORITY_WEIGHTS: &[(&str, i64)] = &[
    ("senior", 12),
    ("lead", 10),
    ("principal", 25),
    ("staff", 20),
    ("manager", 18),
    ("head", 28),
    ("director", 30),
    ("architect", 18),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_and_annualizes_salary() {
        assert_eq!(
            extract_salary("£40k - £50k per year").unwrap().max_gbp,
            50_000
        );
        assert_eq!(extract_salary("£20 per hour").unwrap().min_gbp, 41_600);
        assert_eq!(extract_salary("$60k").unwrap().min_gbp, 47_400);
    }
    #[test]
    fn recognizes_remote_fit_and_lockout() {
        let profile = ResumeProfile {
            remote: true,
            preferred_locations: vec!["uk".into()],
            ..ResumeProfile::default()
        };
        assert!(location_fit("Remote UK", &profile, &[]).0);
        assert!(!location_fit("Remote US only", &profile, &["us only".into()]).0);
    }
}
