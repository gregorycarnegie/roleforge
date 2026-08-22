use quick_xml::escape::unescape;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
};
use url::Url;

static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid tag regex"));

pub fn clean(value: &str) -> String {
    let html = value.replace("&nbsp;", " ");
    let decoded = unescape(&html).map_or(html.clone(), |text| text.into_owned());
    TAGS.replace_all(&decoded, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn normalize(value: &str) -> String {
    clean(value).to_lowercase()
}

/// `value` unless it is empty, in which case `fallback`.
pub fn or_fallback(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value
    }
}

/// Compiled phrase patterns, keyed on the normalized phrase. Scoring one job
/// tests the same few dozen phrases against several fields, and the phrase set
/// is fixed by the resume and the weight tables, so this stops growing after
/// the first job. Building these was ~70% of scoring CPU before it was cached.
///
/// ponytail: one global lock, per-thread caches if scoring ever goes parallel.
static PHRASE_PATTERNS: LazyLock<Mutex<HashMap<String, Regex>>> = LazyLock::new(Mutex::default);

pub fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    let phrase = normalize(phrase);
    if phrase.is_empty() {
        return false;
    }
    let haystack = normalize(haystack);
    let mut patterns = PHRASE_PATTERNS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    patterns
        .entry(phrase)
        .or_insert_with_key(|phrase| {
            let pattern = format!(
                r"(?i)(?:^|\W){}(?:$|\W)",
                regex::escape(phrase).replace(r"\ ", r"\s+")
            );
            // The phrase is escaped, so this cannot fail.
            Regex::new(&pattern).expect("escaped phrase is a valid pattern")
        })
        .is_match(&haystack)
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

/// A colon can also introduce a qualifier ("Remote: IT Support Engineer"), so
/// the left half only counts as a company when it reads like one.
fn looks_like_company(value: &str) -> bool {
    !value.is_empty()
        && value.split_whitespace().count() <= 5
        && !matches!(
            normalize(value).as_str(),
            "remote"
                | "hybrid"
                | "onsite"
                | "on-site"
                | "new"
                | "urgent"
                | "hiring"
                | "job"
                | "featured"
                | "contract"
                | "full time"
                | "part time"
        )
}

/// Feeds spell the pairing two ways: "Role at Company" and "Company: Role".
/// The colon form is tried first, being the less ambiguous of the two.
pub fn split_title_company(value: &str) -> (String, String) {
    let title = clean(value);
    if let Some((company, role)) = title.split_once(": ")
        && looks_like_company(company.trim())
        && !role.trim().is_empty()
    {
        return (role.trim().to_owned(), company.trim().to_owned());
    }
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

static COMPANY_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[,\s]+(?:ltd\.?|limited|inc\.?|llc|plc|corp\.?|corporation)$")
        .expect("valid suffix regex")
});

pub fn normalize_company(value: &str) -> String {
    COMPANY_SUFFIX
        .replace(normalize(value).trim(), "")
        .trim()
        .to_owned()
}

fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
        assert_eq!(clean("  spaced \n out  "), "spaced out");
        assert_eq!(clean("a &amp; b"), "a & b");
        assert_eq!(normalize("<b>IT Support</b>"), "it support");
        assert!(contains_phrase(
            "Senior IT   Support Engineer",
            "it support"
        ));
        assert!(!contains_phrase("JavaScript", "java"));
        assert!(!contains_phrase("anything", ""));
        assert!(contains_phrase("Azure/Intune estate", "intune"));
    }

    #[test]
    fn or_fallback_substitutes_only_for_an_empty_string() {
        assert_eq!(or_fallback(String::new(), "fallback"), "fallback");
        assert_eq!(or_fallback("kept".to_owned(), "fallback"), "kept");
    }

    #[test]
    fn find_matches_keeps_order_dedupes_and_honours_the_limit() {
        let phrases = ["intune".to_owned(), "azure".to_owned(), "intune".to_owned()];
        assert_eq!(
            find_matches("Azure and Intune estate", &phrases, 0),
            ["intune", "azure"]
        );
        assert_eq!(
            find_matches("Azure and Intune estate", &phrases, 1),
            ["intune"]
        );
        assert_eq!(
            find_matches("Azure and Intune estate", &phrases, 5),
            ["intune", "azure"]
        );
        assert!(find_matches("Nothing here", &phrases, 3).is_empty());
        assert!(find_matches("Azure", &[], 3).is_empty());
    }

    #[test]
    fn dedupe_trims_drops_blanks_and_ignores_case() {
        assert_eq!(
            dedupe([
                " Acme ".to_owned(),
                "acme".to_owned(),
                String::new(),
                "   ".to_owned(),
                "Globex".to_owned()
            ]),
            ["Acme", "Globex"]
        );
        assert!(dedupe([]).is_empty());
    }

    #[test]
    fn strips_tracking_from_fingerprints() {
        assert_eq!(
            normalized_link("HTTPS://Example.com/jobs/1/?utm_source=x&keep=y#top"),
            "https://example.com/jobs/1?keep=y"
        );
        assert_eq!(
            normalized_link("https://example.com/jobs/1?ref=a&gclid=b&fbclid=c"),
            "https://example.com/jobs/1"
        );
        // A bare path collapses to the root rather than an empty path.
        assert_eq!(
            normalized_link("https://example.com/"),
            "https://example.com"
        );
        // Anything unparseable is passed through trimmed.
        assert_eq!(normalized_link("  not a url  "), "not a url");
    }

    #[test]
    fn splits_company_and_normalizes_suffix() {
        assert_eq!(
            split_title_company("Engineer at Acme Ltd"),
            ("Engineer".into(), "Acme Ltd".into())
        );
        assert_eq!(
            split_title_company("Engineer | Acme"),
            ("Engineer".into(), "Acme".into())
        );
        assert_eq!(
            split_title_company("Engineer - Acme"),
            ("Engineer".into(), "Acme".into())
        );
        // We Work Remotely and friends put the company first, behind a colon.
        assert_eq!(
            split_title_company("Ignition, Inc.: Mac MSP Help Desk Guru (work from home)"),
            (
                "Mac MSP Help Desk Guru (work from home)".into(),
                "Ignition, Inc.".into()
            )
        );
        // A qualifier or a sentence in front of the colon is not a company.
        assert_eq!(
            split_title_company("Remote: IT Support Engineer"),
            ("Remote: IT Support Engineer".into(), String::new())
        );
        assert_eq!(
            split_title_company("Now hiring for our London office: Engineer"),
            (
                "Now hiring for our London office: Engineer".into(),
                String::new()
            )
        );
        // A separator with nothing on one side is not a split.
        assert_eq!(
            split_title_company("Engineer at "),
            ("Engineer at".into(), String::new())
        );
        assert_eq!(
            split_title_company("Engineer"),
            ("Engineer".into(), String::new())
        );

        assert_eq!(normalize_company("Acme Limited"), "acme");
        assert_eq!(normalize_company("Acme, Inc."), "acme");
        assert_eq!(normalize_company("Acme PLC"), "acme");
        // Only a trailing suffix is stripped.
        assert_eq!(normalize_company("Limited Edition Ltd"), "limited edition");
        assert_eq!(normalize_company("Acme"), "acme");
    }

    #[test]
    fn review_fingerprints_prefer_link_then_role_then_text() {
        let both = review_fingerprints(
            "Engineer at Acme Ltd",
            "Description",
            "https://example.com/jobs/1?utm_source=x",
        );
        assert_eq!(both.len(), 2);
        assert!(both[0].starts_with("link:"), "{both:?}");
        assert!(both[1].starts_with("role_company:"), "{both:?}");

        // Tracking params and company suffixes must not change the digests.
        assert_eq!(
            both,
            review_fingerprints(
                "Engineer at Acme Limited",
                "Other",
                "https://example.com/jobs/1"
            )
        );

        // No company in the title falls back to a title+description digest.
        let text_only = review_fingerprints("Engineer", "Description", "");
        assert_eq!(text_only.len(), 1);
        assert!(text_only[0].starts_with("text:"), "{text_only:?}");
        assert_ne!(text_only, review_fingerprints("Engineer", "Different", ""));

        // Nothing to fingerprint at all.
        assert!(review_fingerprints("", "", "").is_empty());
    }

    #[test]
    fn digest_is_lowercase_hex_and_input_sensitive() {
        let hash = digest("roleforge");
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_eq!(
            hash,
            "149e6280edfca862fb7d4ad945f11f038b2cc166543039c33a58c5b8f4986a48"
        );
        assert_ne!(digest("roleforge"), digest("roleforg"));
    }

    #[test]
    fn truncate_cuts_on_a_word_boundary() {
        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate("  <b>short</b>  ", 20), "short");
        assert_eq!(truncate("one two three four", 12), "one two…");
        // No space to break on: the cut lands mid-word.
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("exactly ten", 11), "exactly ten");
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
}
