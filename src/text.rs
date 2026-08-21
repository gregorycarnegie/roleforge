use quick_xml::escape::unescape;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::OnceLock;
use url::Url;

fn tags() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[^>]+>").expect("valid tag regex"))
}

pub fn clean(value: &str) -> String {
    let html = value.replace("&nbsp;", " ");
    let decoded = unescape(&html).map_or(html.clone(), |text| text.into_owned());
    tags()
        .replace_all(&decoded, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn normalize(value: &str) -> String {
    clean(value).to_lowercase()
}

pub fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    let phrase = normalize(phrase);
    if phrase.is_empty() {
        return false;
    }
    let pattern = format!(
        r"(?i)(?:^|\W){}(?:$|\W)",
        regex::escape(&phrase).replace(r"\ ", r"\s+")
    );
    Regex::new(&pattern).is_ok_and(|re| re.is_match(&normalize(haystack)))
}

pub fn find_matches(text: &str, phrases: &[String], limit: usize) -> Vec<String> {
    let mut found = vec![];
    for phrase in phrases {
        if contains_phrase(text, phrase) && !found.contains(phrase) {
            found.push(phrase.clone());
            if limit > 0 && found.len() == limit {
                break;
            }
        }
    }
    found
}

pub fn dedupe(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && seen.insert(value.to_lowercase()))
        .collect()
}

pub fn normalized_link(value: &str) -> String {
    let Ok(mut url) = Url::parse(value.trim()) else {
        return value.trim().to_owned();
    };
    let tracking = [
        "ref", "source", "src", "campaign", "email", "mc_cid", "mc_eid", "fbclid", "gclid", "yclid",
    ];
    let pairs: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| !key.starts_with("utm_") && !tracking.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    url.set_fragment(None);
    url.to_string().trim_end_matches('/').to_owned()
}

pub fn split_title_company(value: &str) -> (String, String) {
    let title = clean(value);
    for separator in [" at ", " | ", " - ", " — ", " – "] {
        if let Some((left, right)) = title.split_once(separator)
            && !left.trim().is_empty()
            && !right.trim().is_empty()
        {
            return (left.trim().to_owned(), right.trim().to_owned());
        }
    }
    (title, String::new())
}

pub fn normalize_company(value: &str) -> String {
    static SUFFIX: OnceLock<Regex> = OnceLock::new();
    let re = SUFFIX.get_or_init(|| {
        Regex::new(r"(?i)[,\s]+(?:ltd\.?|limited|inc\.?|llc|plc|corp\.?|corporation)$")
            .expect("valid suffix regex")
    });
    re.replace(normalize(value).trim(), "").trim().to_owned()
}

fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
            hex
        })
}

pub fn review_fingerprints(title: &str, description: &str, link: &str) -> Vec<String> {
    let mut values = vec![];
    let link = normalized_link(link);
    if !link.is_empty() {
        values.push(format!("link:{}", digest(&link)));
    }
    let (role, company) = split_title_company(title);
    if !role.is_empty() && !company.is_empty() {
        values.push(format!(
            "role_company:{}",
            digest(&format!(
                "{}|{}",
                normalize(&role),
                normalize_company(&company)
            ))
        ));
    } else {
        let text = format!("{}|{}", normalize(title), normalize(description));
        if text != "|" {
            values.push(format!("text:{}", digest(&text)));
        }
    }
    values
}

pub fn truncate(value: &str, limit: usize) -> String {
    let value = clean(value);
    if value.chars().count() <= limit {
        return value;
    }
    let shortened: String = value.chars().take(limit.saturating_sub(1)).collect();
    let boundary = shortened.rfind(' ').unwrap_or(shortened.len());
    format!("{}…", shortened[..boundary].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_and_matches_whole_phrases() {
        assert_eq!(clean("<b>IT&nbsp; support</b>"), "IT support");
        assert!(contains_phrase(
            "Senior IT   Support Engineer",
            "it support"
        ));
        assert!(!contains_phrase("JavaScript", "java"));
    }

    #[test]
    fn strips_tracking_from_fingerprints() {
        assert_eq!(
            normalized_link("HTTPS://Example.com/jobs/1/?utm_source=x&keep=y#top"),
            "https://example.com/jobs/1?keep=y"
        );
    }

    // Properties, not examples: these hold for every input, and the fingerprint
    // and digest paths depend on them for arbitrary scraped text.
    proptest::proptest! {
        // Dedup keys come from normalized_link, so a second pass must be a no-op —
        // otherwise the same job fingerprints two ways and slips past the filter.
        #[test]
        fn normalized_link_is_idempotent(value in r"\PC{0,80}") {
            let once = normalized_link(&value);
            proptest::prop_assert_eq!(normalized_link(&once), once.clone());
        }

        // clean() output is hashed and rendered; stray or doubled whitespace
        // would split otherwise-identical strings into separate fingerprints.
        #[test]
        fn clean_collapses_whitespace(value in r"\PC{0,200}") {
            let cleaned = clean(&value);
            proptest::prop_assert_eq!(cleaned.trim(), &cleaned);
            proptest::prop_assert!(!cleaned.contains("  "));
        }

        // Telegram rejects over-long messages, so the cap has to hold for every
        // input, ellipsis included.
        #[test]
        fn truncate_respects_limit(value in r"\PC{0,200}", limit in 1usize..100) {
            proptest::prop_assert!(truncate(&value, limit).chars().count() <= limit);
        }
    }

    #[test]
    fn splits_company_and_normalizes_suffix() {
        assert_eq!(
            split_title_company("Engineer at Acme Ltd"),
            ("Engineer".into(), "Acme Ltd".into())
        );
        assert_eq!(normalize_company("Acme Limited"), "acme");
    }
}
