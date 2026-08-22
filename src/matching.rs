use crate::config::MIN_MATCH_SCORE;
use crate::model::{JobLead, MatchResult, ResumeProfile, RoleProfile, SearchConfig};
use crate::text::{
    clean, contains_phrase, dedupe, find_matches, normalize, or_fallback, split_title_company,
    truncate,
};
use regex::Regex;
use serde_json::json;
use std::sync::LazyLock;

#[derive(Clone, Debug, PartialEq)]
pub struct SalaryInfo {
    pub min_gbp: i64,
    pub max_gbp: i64,
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
    } else if profile.hybrid && local && !locked && (hybrid || !remote) {
        (true, "location fit: local/hybrid compatible")
    } else if profile.onsite && local && !locked && (onsite || !remote) {
        (true, "location fit: local/onsite compatible")
    } else if locked {
        (false, "location reject: explicit geographic lockout")
    } else {
        (false, "location reject: not aligned with preferences")
    }
}

/// Conversion rate into GBP. The currency name itself is not scored on.
fn currency(token: &str) -> Option<f64> {
    match token.trim().to_lowercase().as_str() {
        "£" | "gbp" | "pound" | "pounds" => Some(1.0),
        "$" | "us$" | "usd" | "dollar" | "dollars" => Some(0.79),
        "€" | "eur" | "euro" | "euros" => Some(0.86),
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

/// Each pattern paired with whether it matches a range and whether the currency
/// token comes before the figures. Tried in order; the first match wins.
static SALARY_PATTERNS: LazyLock<[(Regex, bool, bool); 4]> = LazyLock::new(|| {
    let compile = |pattern| Regex::new(pattern).expect("valid salary regex");
    [
        (
            compile(
                r"(?i)(£|gbp|us\$|usd|\$|eur|€)\s*(\d[\d,]*(?:\.\d+)?)\s*(k)?\s*(?:-|–|—|to)\s*(?:£|gbp|us\$|usd|\$|eur|€)?\s*(\d[\d,]*(?:\.\d+)?)\s*(k)?",
            ),
            true,
            true,
        ),
        (
            compile(
                r"(?i)(\d[\d,]*(?:\.\d+)?)\s*(k)?\s*(?:-|–|—|to)\s*(\d[\d,]*(?:\.\d+)?)\s*(k)?\s*(gbp|usd|eur|pounds?|dollars?|euros?)",
            ),
            true,
            false,
        ),
        (
            compile(r"(?i)(£|gbp|us\$|usd|\$|eur|€)\s*(\d[\d,]*(?:\.\d+)?)\s*(k)?"),
            false,
            true,
        ),
        (
            compile(r"(?i)(\d[\d,]*(?:\.\d+)?)\s*(k)?\s*(gbp|usd|eur|pounds?|dollars?|euros?)"),
            false,
            false,
        ),
    ]
});

pub fn extract_salary(text: &str) -> Option<SalaryInfo> {
    let normalized = clean(text).to_lowercase();
    for &(ref regex, range, prefix) in SALARY_PATTERNS.iter() {
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
        let rate = currency(currency_text)?;
        let matched = caps.get(0)?;
        let end = (matched.end() + 32).min(normalized.len());
        let (cadence, multiplier) = cadence(&normalized[matched.start()..end]);
        if cadence == "year" && high < 1000.0 {
            continue;
        }
        return Some(SalaryInfo {
            min_gbp: (low.min(high) * rate * multiplier).round() as i64,
            max_gbp: (low.max(high) * rate * multiplier).round() as i64,
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
            candidate: None,
        };
    }

    let company = if item.company.trim().is_empty() {
        or_fallback(split_title_company(&title).1, source)
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
            candidate: None,
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
    MatchResult {
        qualified: score >= MIN_MATCH_SCORE,
        score,
        candidate: Some(candidate),
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

    fn profile() -> ResumeProfile {
        ResumeProfile {
            remote: true,
            hybrid: true,
            onsite: true,
            minimum_salary_gbp: 40_000,
            preferred_locations: vec!["london".into()],
            target_roles: vec!["it support engineer".into(), "service desk analyst".into()],
            skills: vec!["intune".into(), "active directory".into(), "azure".into()],
            competencies: vec!["stakeholder management".into()],
            evidence: vec![
                ("Endpoint".into(), "Rolled out Intune to 900 devices".into()),
                ("Unrelated".into(), "Ran the office coffee rota".into()),
            ],
            ..ResumeProfile::default()
        }
    }

    fn role(name: &str, title: &str, desc: &str, boosts: (i64, i64, i64)) -> RoleProfile {
        RoleProfile {
            name: name.into(),
            display_name: String::new(),
            title_keywords: vec![title.into()],
            description_keywords: vec![desc.into()],
            title_boost: boosts.0,
            description_boost: boosts.1,
            priority: boosts.2,
        }
    }

    fn search() -> SearchConfig {
        SearchConfig {
            company_whitelist: vec!["acme".into()],
            company_blacklist: vec!["spamcorp".into()],
            priority_companies: vec!["globex".into()],
            role_profiles: vec![RoleProfile {
                display_name: "IT Support".into(),
                ..role("support", "it support", "intune", (9, 3, 70))
            }],
            ..SearchConfig::default()
        }
    }

    fn lead(title: &str, description: &str) -> JobLead {
        JobLead {
            title: title.into(),
            link: "https://example.com/jobs/1".into(),
            company: "Acme Ltd".into(),
            description: description.into(),
            ..JobLead::default()
        }
    }

    /// The reasons production actually ships, read back off the candidate.
    fn reasons(result: &MatchResult) -> Vec<String> {
        result
            .candidate
            .as_ref()
            .map_or_else(Vec::new, |candidate| {
                candidate["reasons"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str())
                    .map(str::to_owned)
                    .collect()
            })
    }

    fn score(item: &JobLead) -> MatchResult {
        score_job(
            item,
            "feed",
            &profile(),
            &search(),
            "2026-01-01T00:00:00Z",
            &[],
        )
    }

    // --- salary ----------------------------------------------------------

    #[test]
    fn parses_and_annualizes_salary() {
        // Every currency and cadence is observable through the annualized GBP
        // figures alone, which is all that scoring reads.
        let annual = extract_salary("£40k - £50k per year").unwrap();
        assert_eq!((annual.min_gbp, annual.max_gbp), (40_000, 50_000));

        let hourly = extract_salary("£20 - £25 per hour").unwrap();
        assert_eq!((hourly.min_gbp, hourly.max_gbp), (41_600, 52_000));

        // A non-unit rate and multiplier together: catches a swapped * for /.
        let usd = extract_salary("$60k - $80k").unwrap();
        assert_eq!((usd.min_gbp, usd.max_gbp), (47_400, 63_200));

        let monthly = extract_salary("5500 eur per month").unwrap();
        assert_eq!(monthly.min_gbp, 56_760);

        let daily = extract_salary("£450 per day").unwrap();
        assert_eq!(daily.min_gbp, 117_000);
    }

    #[test]
    fn rejects_unusable_salary_text() {
        // An annual figure under £1000 is a typo or a fee, not a salary.
        assert_eq!(extract_salary("£950 per year"), None);
        assert_eq!(extract_salary("£1200 per year").unwrap().min_gbp, 1_200);
        // The floor is exclusive: exactly £1000 a year still counts.
        assert_eq!(extract_salary("£1000 per year").unwrap().min_gbp, 1_000);
        assert_eq!(extract_salary("competitive salary"), None);
        // Cadence is read from a window after the match, not the whole string:
        // "per hour" falls outside it, so £45k stays an annual figure.
        assert_eq!(
            extract_salary("£45k for the role. Interviews are held per hour, weekly.")
                .unwrap()
                .min_gbp,
            45_000
        );
    }

    #[test]
    fn reads_every_supported_currency_token() {
        assert_eq!(currency("GBP"), Some(1.0));
        assert_eq!(currency(" £ "), Some(1.0));
        assert_eq!(currency("us$"), Some(0.79));
        assert_eq!(currency("euros"), Some(0.86));
        assert_eq!(currency("€"), Some(0.86));
        assert_eq!(currency("btc"), None);
    }

    #[test]
    fn cadence_multipliers_cover_every_period() {
        assert_eq!(cadence("paid hourly"), ("hour", 2080.0));
        assert_eq!(cadence("£400 day rate"), ("day", 260.0));
        assert_eq!(cadence("per month"), ("month", 12.0));
        assert_eq!(cadence("nothing stated"), ("year", 1.0));
    }

    #[test]
    fn amount_applies_the_thousands_suffix() {
        assert_eq!(amount("40", Some("k")), Some(40_000.0));
        assert_eq!(amount("40", None), Some(40.0));
        assert_eq!(amount("1,250", None), Some(1250.0));
        assert_eq!(amount("abc", None), None);
    }

    // --- location --------------------------------------------------------

    #[test]
    fn recognizes_remote_fit_and_lockout() {
        let remote_only = ResumeProfile {
            remote: true,
            preferred_locations: vec!["uk".into()],
            ..ResumeProfile::default()
        };
        assert_eq!(
            location_fit("Remote UK", &remote_only, &[]),
            (true, "location fit: remote compatible")
        );
        assert_eq!(
            location_fit("Remote US only", &remote_only, &["us only".into()]),
            (false, "location reject: explicit geographic lockout")
        );
        assert_eq!(
            location_fit("Onsite in Berlin", &remote_only, &[]),
            (false, "location reject: not aligned with preferences")
        );
        // "wfh" and "anywhere" are remote too; "remotely" is not a whole phrase.
        assert!(location_fit("WFH available", &remote_only, &[]).0);
        assert!(location_fit("Work from anywhere", &remote_only, &[]).0);
    }

    #[test]
    fn relocation_accepts_anything() {
        let anywhere = ResumeProfile {
            relocation: true,
            ..ResumeProfile::default()
        };
        assert_eq!(
            location_fit("Onsite in Tokyo, US only", &anywhere, &["us only".into()]),
            (true, "location fit: relocation allowed")
        );
    }

    #[test]
    fn hybrid_and_onsite_need_a_preferred_location() {
        let hybrid = ResumeProfile {
            hybrid: true,
            preferred_locations: vec!["london".into()],
            ..ResumeProfile::default()
        };
        assert_eq!(
            location_fit("Hybrid role, remote friendly, London", &hybrid, &[]),
            (true, "location fit: local/hybrid compatible")
        );
        // No hybrid wording and no remote wording: still local, so still a fit.
        assert_eq!(
            location_fit("London based role", &hybrid, &[]),
            (true, "location fit: local/hybrid compatible")
        );
        // Remote-only wording with no hybrid wording is not a hybrid fit.
        assert_eq!(
            location_fit("Fully remote, anywhere", &hybrid, &[]),
            (false, "location reject: not aligned with preferences")
        );
        // Hybrid, but somewhere else.
        assert_eq!(
            location_fit("Hybrid role in Manchester", &hybrid, &[]),
            (false, "location reject: not aligned with preferences")
        );

        let onsite = ResumeProfile {
            onsite: true,
            preferred_locations: vec!["london".into()],
            ..ResumeProfile::default()
        };
        assert_eq!(
            location_fit("On-site, London, remote occasionally", &onsite, &[]),
            (true, "location fit: local/onsite compatible")
        );
        assert_eq!(
            location_fit("Fully remote, anywhere", &onsite, &[]),
            (false, "location reject: not aligned with preferences")
        );
        // Neither onsite nor remote wording: local alone carries it.
        assert_eq!(
            location_fit("London based role", &onsite, &[]),
            (true, "location fit: local/onsite compatible")
        );
        // A profile with none of the three modes set never fits.
        let picky = ResumeProfile {
            preferred_locations: vec!["london".into()],
            ..ResumeProfile::default()
        };
        assert_eq!(
            location_fit("London based role", &picky, &[]),
            (false, "location reject: not aligned with preferences")
        );
    }

    // Regression: the lockout used to gate only the remote branch, so a job
    // restricted to another region was accepted as "local/hybrid compatible"
    // whenever the posting also named a preferred location.
    #[test]
    fn lockout_overrides_hybrid_and_onsite_fit() {
        let text = "Hybrid role in our London office. Remote US only.";
        let lockouts = ["us only".to_owned(), "remote us".to_owned()];
        for profile in [
            ResumeProfile {
                hybrid: true,
                preferred_locations: vec!["london".into()],
                ..ResumeProfile::default()
            },
            ResumeProfile {
                onsite: true,
                preferred_locations: vec!["london".into()],
                ..ResumeProfile::default()
            },
        ] {
            assert_eq!(
                location_fit(text, &profile, &lockouts),
                (false, "location reject: explicit geographic lockout")
            );
        }
    }

    #[test]
    fn location_only_reads_the_first_1000_characters() {
        let profile = ResumeProfile {
            remote: true,
            ..ResumeProfile::default()
        };
        let padded = format!("{}remote", "x ".repeat(600));
        assert!(!location_fit(&padded, &profile, &[]).0);
        assert!(location_fit("remote", &profile, &[]).0);
    }

    // --- company ---------------------------------------------------------

    #[test]
    fn company_lists_rank_blacklist_over_priority_over_whitelist() {
        let config = SearchConfig {
            company_whitelist: vec!["acme".into()],
            company_blacklist: vec!["spamcorp".into()],
            priority_companies: vec!["globex".into()],
            ..SearchConfig::default()
        };
        assert_eq!(
            company_preference("SpamCorp", &config),
            (
                false,
                0,
                "blacklist",
                false,
                "company reject: blacklisted employer spamcorp".to_owned()
            )
        );
        assert_eq!(
            company_preference("Globex", &config),
            (
                true,
                14,
                "priority",
                true,
                "company shortlist: globex".to_owned()
            )
        );
        assert_eq!(
            company_preference("Acme", &config),
            (
                true,
                8,
                "whitelist",
                false,
                "company whitelist: acme".to_owned()
            )
        );
        assert_eq!(
            company_preference("Someone Else", &config),
            (true, 0, "none", false, String::new())
        );
        // Blacklist wins when a company sits on more than one list.
        let both = SearchConfig {
            company_blacklist: vec!["acme".into()],
            priority_companies: vec!["acme".into()],
            ..config
        };
        assert_eq!(company_preference("Acme", &both).2, "blacklist");
    }

    // --- role profiles ---------------------------------------------------

    #[test]
    fn best_role_profile_prefers_the_larger_delta_then_priority() {
        let profiles = vec![
            role("weak", "support", "intune", (2, 1, 99)),
            role("strong", "it support", "intune", (9, 3, 10)),
        ];
        let (chosen, titles, descriptions, delta) =
            best_role_profile("IT Support Engineer", "Intune rollout", &profiles).unwrap();
        assert_eq!(chosen.name, "strong");
        assert_eq!(titles, ["it support"]);
        assert_eq!(descriptions, ["intune"]);
        assert_eq!(delta, 12);

        // Equal deltas fall through to priority.
        let tied = vec![
            role("low", "support", "intune", (5, 5, 1)),
            role("high", "support", "intune", (5, 5, 2)),
        ];
        assert_eq!(
            best_role_profile("Support", "Intune", &tied)
                .unwrap()
                .0
                .name,
            "high"
        );

        // A keyword matched in the title is not counted again in the description.
        let (_, titles, descriptions, delta) = best_role_profile(
            "Intune Engineer",
            "Intune everywhere",
            &[role("dup", "intune", "intune", (9, 3, 0))],
        )
        .unwrap();
        assert_eq!((titles.len(), descriptions.len(), delta), (1, 0, 9));

        assert!(best_role_profile("Chef", "Cooking", &profiles).is_none());
        assert!(best_role_profile("IT Support", "Intune", &[]).is_none());
    }

    #[test]
    fn apply_weights_sums_matches_and_records_at_most_three() {
        let mut reasons = vec![];
        let weights = [("alpha", 3), ("beta", 5), ("gamma", 7), ("delta", 11)];
        let total = apply_weights("alpha beta gamma delta", &weights, &mut reasons, "boost", 1);
        assert_eq!(total, 26);
        assert_eq!(reasons, ["boost: alpha, beta, gamma"]);

        let mut reasons = vec![];
        assert_eq!(
            apply_weights("alpha", &weights, &mut reasons, "penalty", -1),
            -3
        );
        assert_eq!(reasons, ["penalty: alpha"]);

        let mut reasons = vec![];
        assert_eq!(apply_weights("zeta", &weights, &mut reasons, "boost", 1), 0);
        assert!(reasons.is_empty());
    }

    #[test]
    fn push_reason_skips_blanks_and_duplicates() {
        let mut reasons = vec![];
        push_reason(&mut reasons, "first");
        push_reason(&mut reasons, "first");
        push_reason(&mut reasons, "");
        push_reason(&mut reasons, "second");
        assert_eq!(reasons, ["first", "second"]);
    }

    // --- score_job -------------------------------------------------------

    #[test]
    fn scores_a_strong_match_with_full_provenance() {
        let item = JobLead {
            salary: "£45,000 - £55,000 per year".into(),
            location: "London".into(),
            employment_type: "Permanent".into(),
            ..lead(
                "IT Support Engineer",
                "Hybrid in London. Intune and Active Directory estate, plus stakeholder management.",
            )
        };
        let result = score(&item);
        assert!(result.qualified);
        // 8 whitelist + 34 target title + 24 skills + 12 competencies
        // + 30 title boost + 5 role delta + 4 salary fit.
        assert_eq!(result.score, 117);

        let candidate = result.candidate.as_ref().unwrap();
        assert_eq!(candidate["company"], "Acme Ltd");
        assert_eq!(candidate["source"], "feed");
        assert_eq!(candidate["score"], result.score);
        assert_eq!(candidate["company_control"], "whitelist");
        assert_eq!(candidate["shortlisted"], false);
        assert_eq!(candidate["application_ready"], true);
        assert_eq!(candidate["role_profile"], "IT Support");
        assert_eq!(candidate["time"], "2026-01-01T00:00:00Z");
        assert_eq!(candidate["link"], "https://example.com/jobs/1");
        assert_eq!(
            candidate["intro_message"],
            "Hi, I'm interested in the IT Support Engineer role at Acme Ltd. My background aligns well with the requirements."
        );
        // Evidence is filtered to bullets that mention a matched keyword.
        assert_eq!(
            candidate["resume_bullet_suggestions"],
            json!(["Endpoint: Rolled out Intune to 900 devices"])
        );
        assert_eq!(
            candidate["why_this_fits"][0],
            "Matches your it support engineer experience."
        );
        // Reasons are capped at six; the salary note is the seventh here.
        assert_eq!(reasons(&result).len(), 6);
        let joined = reasons(&result).join(" | ");
        assert!(
            joined.contains("location fit: local/hybrid compatible"),
            "{joined}"
        );
        assert!(joined.contains("company whitelist: acme"), "{joined}");
        assert!(
            joined.contains("target role in title: it support engineer"),
            "{joined}"
        );
        assert!(
            joined.contains("skills in description: intune, active directory"),
            "{joined}"
        );
    }

    #[test]
    fn location_reject_short_circuits_before_any_scoring() {
        let item = lead("IT Support Engineer", "Intune. Active Directory. Azure.");
        let profile = ResumeProfile {
            remote: true,
            ..ResumeProfile::default()
        };
        let result = score_job(
            &item,
            "feed",
            &profile,
            &search(),
            "2026-01-01T00:00:00Z",
            &[],
        );
        // The reject reason itself is covered where it is produced, by the
        // location_fit tests above.
        assert!(!result.qualified);
        assert_eq!(result.score, 0);
        assert!(result.candidate.is_none());
    }

    #[test]
    fn blacklisted_company_short_circuits_after_location() {
        let item = JobLead {
            company: "SpamCorp".into(),
            ..lead("IT Support Engineer", "Remote. Intune.")
        };
        let result = score(&item);
        // Scoring stops before any keyword work; company_preference's own
        // tests cover the reject reason.
        assert!(!result.qualified);
        assert_eq!(result.score, 0);
        assert!(result.candidate.is_none());
    }

    #[test]
    fn priority_company_is_shortlisted() {
        let item = JobLead {
            company: "Globex".into(),
            ..lead("IT Support Engineer", "Remote. Intune.")
        };
        let result = score(&item);
        assert!(
            reasons(&result)
                .iter()
                .any(|r| r == "company shortlist: globex")
        );
        let candidate = result.candidate.unwrap();
        assert_eq!(candidate["shortlisted"], true);
        assert_eq!(candidate["company_control"], "priority");
    }

    #[test]
    fn company_falls_back_to_the_title_then_the_source() {
        let from_title = JobLead {
            company: String::new(),
            ..lead("IT Support Engineer at Initech Ltd", "Remote. Intune.")
        };
        let scored = score(&from_title);
        let candidate = scored.candidate.unwrap();
        assert_eq!(candidate["company"], "Initech Ltd");
        assert_eq!(candidate["title"], "IT Support Engineer at Initech Ltd");

        let from_source = JobLead {
            company: "   ".into(),
            ..lead("IT Support Engineer", "Remote. Intune.")
        };
        assert_eq!(score(&from_source).candidate.unwrap()["company"], "feed");
    }

    #[test]
    fn negative_and_seniority_weights_pull_the_score_down() {
        let baseline = score(&lead("IT Support Engineer", "Remote. Intune.")).score;
        let senior = score(&lead("Senior IT Support Engineer", "Remote. Intune."));
        assert_eq!(senior.score, baseline - 12);
        assert!(
            reasons(&senior)
                .iter()
                .any(|r| r == "seniority penalty: senior")
        );

        let wrong_track = score(&lead(
            "Machine Learning Engineer",
            "Remote. Intune. Active Directory.",
        ));
        assert!(!wrong_track.qualified, "score was {}", wrong_track.score);
        assert!(
            reasons(&wrong_track)
                .iter()
                .any(|r| r == "title penalty: machine learning")
        );
    }

    #[test]
    fn salary_below_preference_is_penalized() {
        let low = JobLead {
            salary: "£24,000 per year".into(),
            ..lead("IT Support Engineer", "Remote. Intune.")
        };
        let high = JobLead {
            salary: "£55,000 per year".into(),
            ..lead("IT Support Engineer", "Remote. Intune.")
        };
        assert_eq!(score(&high).score - score(&low).score, 22);
        // A lean profile keeps the salary note inside the six-reason cap.
        let lean = ResumeProfile {
            remote: true,
            minimum_salary_gbp: 40_000,
            ..ResumeProfile::default()
        };
        let plain = SearchConfig::default();
        let note = |item: &JobLead| reasons(&score_job(item, "feed", &lean, &plain, "t", &[]));
        assert_eq!(
            note(&low),
            [
                "location fit: remote compatible",
                "title boost: it support engineer, support engineer",
                "salary penalty: below preference"
            ]
        );
        assert_eq!(
            note(&high),
            [
                "location fit: remote compatible",
                "title boost: it support engineer, support engineer",
                "salary fit: meets preference"
            ]
        );
        // A salary exactly at the preference meets it rather than missing it.
        let exact = JobLead {
            salary: "£40,000 per year".into(),
            ..lead("IT Support Engineer", "Remote. Intune.")
        };
        assert_eq!(
            note(&exact),
            [
                "location fit: remote compatible",
                "title boost: it support engineer, support engineer",
                "salary fit: meets preference"
            ]
        );

        // With no stated preference, salary moves neither score nor reasons.
        let mut no_preference = lean.clone();
        no_preference.minimum_salary_gbp = 0;
        let quiet =
            |item: &JobLead| reasons(&score_job(item, "feed", &no_preference, &plain, "t", &[]));
        for item in [&low, &high, &exact] {
            assert_eq!(
                quiet(item),
                [
                    "location fit: remote compatible",
                    "title boost: it support engineer, support engineer"
                ],
                "salary reasons leaked with no preference set"
            );
        }
    }

    #[test]
    fn keyword_tallies_scale_with_the_number_of_matches() {
        let counted = ResumeProfile {
            remote: true,
            target_roles: vec!["alpha one".into(), "beta two".into(), "gamma three".into()],
            skills: vec!["delta".into(), "epsilon".into()],
            competencies: vec!["zeta".into(), "eta".into()],
            ..ResumeProfile::default()
        };
        let item = lead(
            "alpha one delta epsilon",
            "Remote. alpha one, beta two and gamma three. zeta and eta.",
        );
        let result = score_job(&item, "feed", &counted, &SearchConfig::default(), "t", &[]);
        // 34 (one target role in the title, no per-extra bonus)
        // + 18 (two more in the description, capped)
        // + 12 (two skills in the title) + 6 (two competencies).
        assert_eq!(result.score, 70);
        assert_eq!(
            reasons(&result),
            [
                "location fit: remote compatible",
                "target role in title: alpha one",
                // "alpha one" also appears in the description but is not recounted.
                "target role in description: beta two, gamma three",
                "skills in title: delta, epsilon",
                "competencies matched: zeta, eta"
            ]
        );
    }

    #[test]
    fn repeated_keywords_are_capped_and_deduplicated() {
        let mut wide = profile();
        wide.target_roles = (0..6).map(|i| format!("role{i}")).collect();
        let title = (0..6)
            .map(|i| format!("role{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let item = lead(&title, "Remote.");
        let capped = score_job(&item, "feed", &wide, &search(), "t", &[]);
        // find_matches stops at 3, so the title bonus saturates at 34 + 7 * 2.
        assert!(
            reasons(&capped)
                .iter()
                .any(|r| r == "target role in title: role0, role1, role2"),
            "{:?}",
            reasons(&capped)
        );
        assert_eq!(capped.score, 56);
    }

    #[test]
    fn qualification_tracks_the_minimum_score() {
        let weak = score(&lead("IAM Coordinator", "Remote."));
        assert!(!weak.qualified);
        assert!(weak.score < MIN_MATCH_SCORE);
        let candidate = weak.candidate.unwrap();
        assert_eq!(candidate["application_ready"], false);
        assert_eq!(candidate["role_profile"], "");
    }
}
