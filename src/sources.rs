use crate::model::{JobLead, object, string};
use crate::text::clean;
use crate::{Error, Result};
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::Regex;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::env;
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;

const USER_AGENT: &str = "RoleForge/0.1 (+https://github.com/gregorycarnegie/roleforge)";

fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()?)
}

fn fetch_text(request: RequestBuilder) -> Result<String> {
    Ok(request.send()?.error_for_status()?.text()?)
}

fn fetch_json(request: RequestBuilder) -> Result<Value> {
    Ok(request.send()?.error_for_status()?.json()?)
}

fn cfg(config: &Value, key: &str) -> String {
    string(config.get(key))
}
fn display_name(config: &Value) -> String {
    let value = cfg(config, "display_name");
    if value.is_empty() {
        cfg(config, "name")
    } else {
        value
    }
}
fn value_text(value: Option<&Value>) -> String {
    value
        .map(|value| match value {
            Value::String(text) => clean(text),
            Value::Null => String::new(),
            other => clean(&other.to_string()),
        })
        .unwrap_or_default()
}
fn join(parts: impl IntoIterator<Item = String>) -> String {
    parts
        .into_iter()
        .map(|part| clean(&part))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn first(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .map(|key| value_text(value.get(*key)))
        .find(|item| !item.is_empty())
        .unwrap_or_default()
}

pub fn fetch(config: &Value) -> Result<Vec<JobLead>> {
    match cfg(config, "platform").as_str() {
        "greenhouse" => greenhouse(config),
        "lever" => lever(config),
        "ashby" => ashby(config),
        "workable" => workable(config),
        "generic_html" => generic_html(config),
        _ => match cfg(config, "type").as_str() {
            "efc_html" => generic_html(config),
            "adzuna" => adzuna(config),
            "reed" => reed(config),
            "jooble" => jooble(config),
            "themuse" => themuse(config),
            "arbeitnow" => arbeitnow(config),
            "remotive" => remotive(config),
            _ => rss(config),
        },
    }
}

pub fn parse_feed(xml: &str, source: &str) -> Result<Vec<JobLead>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut jobs = vec![];
    let mut current: Option<JobLead> = None;
    let mut field = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => {
                let name = String::from_utf8_lossy(tag.local_name().as_ref()).to_lowercase();
                if name == "item" || name == "entry" {
                    current = Some(JobLead {
                        source: source.into(),
                        ..JobLead::default()
                    });
                }
                field = name;
            }
            Ok(Event::Empty(tag)) if current.is_some() => {
                let name = String::from_utf8_lossy(tag.local_name().as_ref()).to_lowercase();
                if name == "link" {
                    for attribute in tag.attributes().flatten() {
                        if attribute.key.local_name().as_ref() == b"href" {
                            current.as_mut().expect("checked").link =
                                clean(&String::from_utf8_lossy(&attribute.value));
                        }
                    }
                }
            }
            Ok(Event::Text(text)) if current.is_some() => {
                let value = clean(&String::from_utf8_lossy(text.as_ref()));
                let job = current.as_mut().expect("checked");
                match field.as_str() {
                    "title" => job.title.push_str(&value),
                    "link" if job.link.is_empty() => job.link.push_str(&value),
                    "description" | "summary" | "content" => {
                        if !job.description.is_empty() {
                            job.description.push(' ');
                        }
                        job.description.push_str(&value);
                    }
                    "company" | "author" => job.company.push_str(&value),
                    "location" => job.location.push_str(&value),
                    "pubdate" | "published" | "updated" => job.date_posted.push_str(&value),
                    _ => {}
                }
            }
            Ok(Event::CData(text)) if current.is_some() => {
                let value = clean(&String::from_utf8_lossy(text.as_ref()));
                let job = current.as_mut().expect("checked");
                match field.as_str() {
                    "title" => job.title.push_str(&value),
                    "link" if job.link.is_empty() => job.link.push_str(&value),
                    "description" | "summary" | "content" => {
                        if !job.description.is_empty() {
                            job.description.push(' ');
                        }
                        job.description.push_str(&value);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(tag)) => {
                let name = String::from_utf8_lossy(tag.local_name().as_ref()).to_lowercase();
                if (name == "item" || name == "entry") && current.is_some() {
                    let job = current.take().expect("checked");
                    if !job.title.is_empty() && !job.link.is_empty() {
                        jobs.push(job);
                    }
                }
                field.clear();
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(error.into()),
            _ => {}
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn rss(config: &Value) -> Result<Vec<JobLead>> {
    let xml = fetch_text(client()?.get(cfg(config, "url")))?;
    let source = display_name(config);
    let source = if source.is_empty() {
        cfg(config, "url")
    } else {
        source
    };
    let context = cfg(config, "context_terms");
    let mut jobs = parse_feed(&xml, &source)?;
    if !context.is_empty() {
        for job in &mut jobs {
            job.description = join([job.description.clone(), context.clone()]);
        }
    }
    Ok(jobs)
}

fn extract_json_ld(html: &str) -> Vec<Value> {
    static SCRIPT: OnceLock<Regex> = OnceLock::new();
    let re = SCRIPT.get_or_init(|| {
        Regex::new(r#"(?is)<script[^>]+type=[\"']application/ld\+json[\"'][^>]*>(.*?)</script>"#)
            .expect("valid json-ld regex")
    });
    re.captures_iter(html)
        .filter_map(|capture| serde_json::from_str(capture.get(1)?.as_str()).ok())
        .collect()
}

fn visit_job_nodes(value: &Value, nodes: &mut Vec<Value>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| visit_job_nodes(item, nodes)),
        Value::Object(map) => {
            let is_job = map.get("@type").is_some_and(|kind| {
                kind == "JobPosting"
                    || kind
                        .as_array()
                        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "JobPosting"))
            });
            if is_job {
                nodes.push(value.clone());
            }
            map.values().for_each(|item| visit_job_nodes(item, nodes));
        }
        _ => {}
    }
}

fn job_from_json_ld(value: &Value, fallback_url: &str, source: &str, company: &str) -> JobLead {
    let hiring = value.get("hiringOrganization").map(object);
    let location = value
        .get("jobLocation")
        .and_then(|value| {
            value
                .as_array()
                .and_then(|items| items.first())
                .or(Some(value))
        })
        .map(object);
    let address = location.and_then(|loc| loc.get("address")).map(object);
    JobLead {
        title: value_text(value.get("title")),
        link: first(value, &["url", "sameAs"]).or_else(fallback_url),
        source: source.into(),
        company: hiring
            .map(|org| value_text(org.get("name")))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| company.into()),
        location: address
            .map(|address| {
                join([
                    value_text(address.get("addressLocality")),
                    value_text(address.get("addressRegion")),
                    value_text(address.get("addressCountry")),
                ])
            })
            .unwrap_or_default(),
        salary: salary_text(value.get("baseSalary")),
        description: value_text(value.get("description")),
        employment_type: value_text(value.get("employmentType")),
        date_posted: value_text(value.get("datePosted")),
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

fn generic_html(config: &Value) -> Result<Vec<JobLead>> {
    let urls: Vec<String> = config
        .get("start_urls")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| vec![cfg(config, "url")]);
    let source = display_name(config);
    let company = display_name(config);
    let allowed_domains: Vec<String> = config
        .get("allowed_domains")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|domain| domain.trim().trim_start_matches("www.").to_lowercase())
        .collect();
    let keywords: Vec<String> = config
        .get("job_link_keywords")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| item.to_lowercase())
                .collect()
        })
        .unwrap_or_else(|| {
            [
                "job", "jobs", "career", "careers", "position", "vacancy", "role", "opening",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        });
    let patterns: Vec<Regex> = config
        .get("job_link_regexes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect();
    let max_pages = config
        .get("max_job_pages")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 60) as usize;
    let mut jobs = vec![];
    let mut links = vec![];
    for url in urls.into_iter().filter(|url| !url.is_empty()).take(10) {
        let html = fetch_text(client()?.get(&url))?;
        let mut nodes = vec![];
        for payload in extract_json_ld(&html) {
            visit_job_nodes(&payload, &mut nodes);
        }
        for node in nodes {
            jobs.push(job_from_json_ld(&node, &url, &source, &company));
        }
        for (link, anchor) in extract_anchor_links(&html, &url) {
            let host_allowed = allowed_domains.is_empty()
                || Url::parse(&link)
                    .ok()
                    .and_then(|url| {
                        url.host_str()
                            .map(|host| host.trim_start_matches("www.").to_lowercase())
                    })
                    .is_some_and(|host| {
                        allowed_domains
                            .iter()
                            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
                    });
            let searchable = format!("{} {}", link.to_lowercase(), anchor.to_lowercase());
            if host_allowed
                && (keywords.iter().any(|keyword| searchable.contains(keyword))
                    || patterns.iter().any(|pattern| pattern.is_match(&link)))
            {
                links.push(link);
            }
        }
    }
    let mut seen = HashSet::new();
    for url in links
        .into_iter()
        .filter(|link| seen.insert(link.clone()))
        .take(max_pages)
    {
        let Ok(html) = fetch_text(client()?.get(&url)) else {
            continue;
        };
        let mut nodes = vec![];
        for payload in extract_json_ld(&html) {
            visit_job_nodes(&payload, &mut nodes);
        }
        if nodes.is_empty() {
            if let Some(job) = fallback_html_job(&html, &url, &source, &company) {
                jobs.push(job);
            }
        } else {
            for node in nodes {
                jobs.push(job_from_json_ld(&node, &url, &source, &company));
            }
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn extract_anchor_links(html: &str, base: &str) -> Vec<(String, String)> {
    static ANCHOR: OnceLock<Regex> = OnceLock::new();
    let regex = ANCHOR.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*[\"']([^\"']+)[\"'][^>]*>(.*?)</a>"#)
            .expect("valid anchor regex")
    });
    let Ok(base) = Url::parse(base) else {
        return vec![];
    };
    regex
        .captures_iter(html)
        .filter_map(|capture| {
            let href = capture.get(1)?.as_str().trim();
            if href.starts_with('#')
                || href.starts_with("mailto:")
                || href.starts_with("javascript:")
            {
                return None;
            }
            Some((
                base.join(href).ok()?.to_string(),
                clean(capture.get(2)?.as_str()),
            ))
        })
        .collect()
}

fn fallback_html_job(html: &str, url: &str, source: &str, company: &str) -> Option<JobLead> {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static DESCRIPTION: OnceLock<Regex> = OnceLock::new();
    let title = TITLE
        .get_or_init(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("valid title regex"))
        .captures(html)
        .and_then(|capture| capture.get(1))
        .map(|value| clean(value.as_str()))
        .unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    let description = DESCRIPTION.get_or_init(|| Regex::new(r#"(?is)<meta[^>]+(?:name|property)=[\"'](?:description|og:description)[\"'][^>]+content=[\"']([^\"']*)[\"']"#).expect("valid meta regex"))
        .captures(html).and_then(|capture| capture.get(1)).map(|value| clean(value.as_str())).unwrap_or_default();
    Some(JobLead {
        title,
        link: url.into(),
        source: source.into(),
        company: company.into(),
        description,
        ..JobLead::default()
    })
}

fn greenhouse(config: &Value) -> Result<Vec<JobLead>> {
    let url = format!(
        "https://boards-api.greenhouse.io/v1/boards/{}/jobs?content=true",
        cfg(config, "board_token")
    );
    let payload = fetch_json(client()?.get(url))?;
    Ok(dedupe_jobs(
        array(&payload, "jobs")
            .iter()
            .map(|job| JobLead {
                title: value_text(job.get("title")),
                link: value_text(job.get("absolute_url")),
                source: "Greenhouse".into(),
                company: display_name(config),
                location: job
                    .get("location")
                    .map(object)
                    .map(|loc| value_text(loc.get("name")))
                    .unwrap_or_default(),
                description: value_text(job.get("content")),
                ..JobLead::default()
            })
            .collect(),
    ))
}

fn lever(config: &Value) -> Result<Vec<JobLead>> {
    let base = if cfg(config, "instance").eq_ignore_ascii_case("eu") {
        "https://api.eu.lever.co"
    } else {
        "https://api.lever.co"
    };
    let mut jobs = vec![];
    for skip in (0..10_000).step_by(100) {
        let url = format!(
            "{base}/v0/postings/{}?mode=json&limit=100&skip={skip}",
            cfg(config, "site")
        );
        let payload = fetch_json(client()?.get(url))?;
        let Some(items) = payload.as_array() else {
            break;
        };
        for job in items {
            let categories = job.get("categories").map(object);
            jobs.push(JobLead {
                title: value_text(job.get("text")),
                link: first(job, &["hostedUrl", "applyUrl"]),
                source: "Lever".into(),
                company: display_name(config),
                location: categories
                    .map(|c| value_text(c.get("location")))
                    .unwrap_or_default(),
                employment_type: categories
                    .map(|c| value_text(c.get("commitment")))
                    .unwrap_or_default(),
                description: join([
                    value_text(job.get("descriptionPlain")),
                    value_text(job.get("descriptionBodyPlain")),
                    value_text(job.get("additionalPlain")),
                ]),
                salary: value_text(job.get("salaryDescriptionPlain")),
                ..JobLead::default()
            });
        }
        if items.len() < 100 {
            break;
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn ashby(config: &Value) -> Result<Vec<JobLead>> {
    let url = format!(
        "https://api.ashbyhq.com/posting-api/job-board/{}?includeCompensation=true",
        cfg(config, "job_board_name")
    );
    let payload = fetch_json(client()?.get(url))?;
    Ok(dedupe_jobs(
        array(&payload, "jobs")
            .iter()
            .filter(|job| job.get("isListed") != Some(&Value::Bool(false)))
            .map(|job| JobLead {
                title: value_text(job.get("title")),
                link: first(job, &["jobUrl", "applyUrl"]),
                source: "Ashby".into(),
                company: display_name(config),
                location: value_text(job.get("location")),
                employment_type: value_text(job.get("employmentType")),
                description: join([
                    value_text(job.get("descriptionPlain")),
                    value_text(job.get("department")),
                    value_text(job.get("team")),
                    value_text(job.get("workplaceType")),
                ]),
                ..JobLead::default()
            })
            .collect(),
    ))
}

fn workable(config: &Value) -> Result<Vec<JobLead>> {
    let subdomain = cfg(config, "account_subdomain");
    let request = if cfg(config, "mode") == "spi" {
        let variable = cfg(config, "api_token_env");
        let token = env::var(&variable).map_err(|_| format!("Workable requires {variable}"))?;
        client()?
            .get(format!("https://{subdomain}.workable.com/spi/v3/jobs"))
            .bearer_auth(token)
    } else {
        client()?.get(format!(
            "https://www.workable.com/api/accounts/{subdomain}?details=true"
        ))
    };
    let payload = fetch_json(request)?;
    let items = payload
        .get("jobs")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    Ok(dedupe_jobs(
        items
            .iter()
            .map(|job| {
                let location = job.get("location").map(object);
                JobLead {
                    title: first(job, &["title", "full_title"]),
                    link: first(job, &["application_url", "url", "shortlink"]),
                    source: "Workable".into(),
                    company: display_name(config),
                    location: location
                        .map(|loc| {
                            join([
                                value_text(loc.get("location_str")),
                                value_text(loc.get("city")),
                                value_text(loc.get("country")),
                            ])
                        })
                        .unwrap_or_default(),
                    description: join([
                        value_text(job.get("description")),
                        value_text(job.get("description_plain")),
                        value_text(job.get("department")),
                    ]),
                    ..JobLead::default()
                }
            })
            .collect(),
    ))
}

fn adzuna(config: &Value) -> Result<Vec<JobLead>> {
    let app_id = required_env("ADZUNA_APP_ID")?;
    let app_key = required_env("ADZUNA_APP_KEY")?;
    let country = cfg(config, "country").or_else("gb");
    let mut jobs = vec![];
    for page in 1..=2 {
        let base = format!("https://api.adzuna.com/v1/api/jobs/{country}/search/{page}");
        let payload = fetch_json(client()?.get(base).query(&[
            ("app_id", &app_id),
            ("app_key", &app_key),
            ("results_per_page", &"50".into()),
            ("what", &cfg(config, "what")),
            ("where", &cfg(config, "where")),
        ]))?;
        let items = array(&payload, "results");
        for job in items {
            jobs.push(JobLead {
                title: value_text(job.get("title")),
                link: value_text(job.get("redirect_url")),
                source: display_name(config),
                company: job
                    .get("company")
                    .map(object)
                    .map(|v| value_text(v.get("display_name")))
                    .unwrap_or_default(),
                location: job
                    .get("location")
                    .map(object)
                    .map(|v| value_text(v.get("display_name")))
                    .unwrap_or_default(),
                salary: format_salary(
                    job.get("salary_min"),
                    job.get("salary_max"),
                    if country == "gb" { "GBP" } else { "USD" },
                    "year",
                ),
                description: value_text(job.get("description")),
                date_posted: value_text(job.get("created")),
                ..JobLead::default()
            });
        }
        if items.len() < 50 {
            break;
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn reed(config: &Value) -> Result<Vec<JobLead>> {
    let key = required_env("REED_APP_KEY")?;
    let payload = fetch_json(
        client()?
            .get("https://www.reed.co.uk/api/1.0/search")
            .basic_auth(key, Some(""))
            .query(&[
                ("resultsToTake", "100"),
                ("keywords", &cfg(config, "keywords")),
                ("locationName", &cfg(config, "location")),
            ]),
    )?;
    Ok(dedupe_jobs(
        array(&payload, "results")
            .iter()
            .map(|job| JobLead {
                title: value_text(job.get("jobTitle")),
                link: value_text(job.get("jobUrl")),
                source: display_name(config),
                company: value_text(job.get("employerName")),
                location: value_text(job.get("locationName")),
                salary: format_salary(
                    job.get("minimumSalary"),
                    job.get("maximumSalary"),
                    "GBP",
                    "year",
                ),
                description: value_text(job.get("jobDescription")),
                date_posted: value_text(job.get("date")),
                ..JobLead::default()
            })
            .collect(),
    ))
}

fn jooble(config: &Value) -> Result<Vec<JobLead>> {
    let key = required_env("JOOBLE_APP_KEY")?;
    let payload = fetch_json(client()?.post(format!("https://jooble.org/api/{key}")).json(&json!({"keywords":cfg(config,"keywords"),"location":cfg(config,"location"),"resultsOnPage":20,"page":1})))?;
    Ok(dedupe_jobs(
        array(&payload, "jobs")
            .iter()
            .map(|job| JobLead {
                title: value_text(job.get("title")),
                link: value_text(job.get("link")),
                source: display_name(config),
                company: value_text(job.get("company")),
                location: value_text(job.get("location")),
                salary: value_text(job.get("salary")),
                description: value_text(job.get("snippet")),
                employment_type: value_text(job.get("type")),
                date_posted: value_text(job.get("updated")),
            })
            .collect(),
    ))
}

fn themuse(config: &Value) -> Result<Vec<JobLead>> {
    let mut jobs = vec![];
    for page in 0..2 {
        let payload = fetch_json(
            client()?
                .get("https://www.themuse.com/api/public/jobs")
                .query(&[
                    ("page", page.to_string()),
                    ("category", cfg(config, "category")),
                    ("descending", "true".into()),
                    ("location", cfg(config, "location")),
                ]),
        )?;
        let items = array(&payload, "results");
        for job in items {
            let locations = array(job, "locations")
                .iter()
                .map(|v| value_text(v.get("name")))
                .collect::<Vec<_>>()
                .join(", ");
            jobs.push(JobLead {
                title: value_text(job.get("name")),
                link: job
                    .get("refs")
                    .map(object)
                    .map(|v| value_text(v.get("landing_page")))
                    .unwrap_or_default(),
                source: display_name(config),
                company: job
                    .get("company")
                    .map(object)
                    .map(|v| value_text(v.get("name")))
                    .unwrap_or_default(),
                location: locations,
                description: value_text(job.get("contents")),
                date_posted: value_text(job.get("publication_date")),
                ..JobLead::default()
            });
        }
        if items.is_empty() {
            break;
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn arbeitnow(config: &Value) -> Result<Vec<JobLead>> {
    let mut jobs = vec![];
    for page in 1..=3 {
        let payload = fetch_json(client()?.get(format!(
            "https://www.arbeitnow.com/api/job-board-api?page={page}"
        )))?;
        let items = array(&payload, "data");
        for job in items {
            jobs.push(JobLead {
                title: value_text(job.get("title")),
                link: value_text(job.get("url")),
                source: display_name(config),
                company: value_text(job.get("company_name")),
                location: value_text(job.get("location")),
                description: value_text(job.get("description")),
                employment_type: job
                    .get("job_types")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|v| value_text(Some(v)))
                    .collect::<Vec<_>>()
                    .join(", "),
                ..JobLead::default()
            });
        }
        if items.is_empty() {
            break;
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn remotive(config: &Value) -> Result<Vec<JobLead>> {
    let payload = fetch_json(
        client()?
            .get("https://remotive.com/api/remote-jobs")
            .query(&[
                ("category", cfg(config, "category")),
                ("limit", "100".into()),
            ]),
    )?;
    Ok(dedupe_jobs(
        array(&payload, "jobs")
            .iter()
            .map(|job| JobLead {
                title: value_text(job.get("title")),
                link: value_text(job.get("url")),
                source: display_name(config),
                company: value_text(job.get("company_name")),
                location: value_text(job.get("candidate_required_location")),
                salary: value_text(job.get("salary")),
                description: value_text(job.get("description")),
                employment_type: value_text(job.get("job_type")),
                date_posted: value_text(job.get("publication_date")),
            })
            .collect(),
    ))
}

fn required_env(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| -> Error { format!("{name} must be set in the environment").into() })
}

fn salary_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let map = object(value);
    let currency = value_text(map.get("currency"));
    let unit = map
        .get("value")
        .map(object)
        .map(|inner| value_text(inner.get("unitText")))
        .unwrap_or_default();
    let inner = map.get("value").unwrap_or(value);
    format_salary(
        inner.get("minValue").or_else(|| inner.get("value")),
        inner.get("maxValue"),
        &currency,
        &unit,
    )
}

fn format_salary(
    min: Option<&Value>,
    max: Option<&Value>,
    currency: &str,
    cadence: &str,
) -> String {
    let number =
        |value: Option<&Value>| value.and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()));
    match (number(min), number(max)) {
        (Some(low), Some(high)) => format!("{currency} {low:.0}-{high:.0} per {cadence}"),
        (Some(value), None) | (None, Some(value)) => format!("{currency} {value:.0} per {cadence}"),
        _ => String::new(),
    }
}

fn dedupe_jobs(jobs: Vec<JobLead>) -> Vec<JobLead> {
    let mut links = HashSet::new();
    jobs.into_iter()
        .filter(|job| {
            !job.title.is_empty() && !job.link.is_empty() && links.insert(job.link.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_rss_and_atom() {
        let rss = r#"<rss><channel><item><title>Support Engineer</title><link>https://example/jobs/1</link><description><![CDATA[Help <b>people</b>]]></description></item></channel></rss>"#;
        let jobs = parse_feed(rss, "Test").unwrap();
        assert_eq!(jobs[0].title, "Support Engineer");
        assert_eq!(jobs[0].description, "Help people");
    }
    #[test]
    fn formats_provider_salary() {
        assert_eq!(
            format_salary(Some(&json!(30_000)), Some(&json!(40_000)), "GBP", "year"),
            "GBP 30000-40000 per year"
        );
    }
    #[test]
    fn resolves_anchor_links() {
        assert_eq!(
            extract_anchor_links(
                r#"<a href='/careers/one'>Engineer</a>"#,
                "https://example.com/jobs"
            )[0],
            ("https://example.com/careers/one".into(), "Engineer".into())
        );
    }
}
