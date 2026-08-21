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
                    // dedupe_jobs drops the untitled and the linkless below.
                    jobs.push(current.take().expect("checked"));
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
    Ok(dedupe_jobs(greenhouse_leads(
        array(&payload, "jobs"),
        config,
    )))
}

fn greenhouse_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
    items
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
        .collect()
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
        jobs.extend(lever_leads(items, config));
        if items.len() < 100 {
            break;
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn lever_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
    items
        .iter()
        .map(|job| {
            let categories = job.get("categories").map(object);
            JobLead {
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
            }
        })
        .collect()
}

fn ashby(config: &Value) -> Result<Vec<JobLead>> {
    let url = format!(
        "https://api.ashbyhq.com/posting-api/job-board/{}?includeCompensation=true",
        cfg(config, "job_board_name")
    );
    let payload = fetch_json(client()?.get(url))?;
    Ok(dedupe_jobs(ashby_leads(array(&payload, "jobs"), config)))
}

fn ashby_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
    items
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
        .collect()
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
    Ok(dedupe_jobs(workable_leads(items, config)))
}

fn workable_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
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
        .collect()
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
        jobs.extend(adzuna_leads(items, config, &country));
        if items.len() < 50 {
            break;
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn adzuna_leads(items: &[Value], config: &Value, country: &str) -> Vec<JobLead> {
    items
        .iter()
        .map(|job| JobLead {
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
        })
        .collect()
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
    Ok(dedupe_jobs(reed_leads(array(&payload, "results"), config)))
}

fn reed_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
    items
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
        .collect()
}

fn jooble(config: &Value) -> Result<Vec<JobLead>> {
    let key = required_env("JOOBLE_APP_KEY")?;
    let payload = fetch_json(client()?.post(format!("https://jooble.org/api/{key}")).json(&json!({"keywords":cfg(config,"keywords"),"location":cfg(config,"location"),"resultsOnPage":20,"page":1})))?;
    Ok(dedupe_jobs(jooble_leads(array(&payload, "jobs"), config)))
}

fn jooble_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
    items
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
        .collect()
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
        jobs.extend(themuse_leads(items, config));
        if items.is_empty() {
            break;
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn themuse_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
    items
        .iter()
        .map(|job| JobLead {
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
            location: array(job, "locations")
                .iter()
                .map(|v| value_text(v.get("name")))
                .collect::<Vec<_>>()
                .join(", "),
            description: value_text(job.get("contents")),
            date_posted: value_text(job.get("publication_date")),
            ..JobLead::default()
        })
        .collect()
}

fn arbeitnow(config: &Value) -> Result<Vec<JobLead>> {
    let mut jobs = vec![];
    for page in 1..=3 {
        let payload = fetch_json(client()?.get(format!(
            "https://www.arbeitnow.com/api/job-board-api?page={page}"
        )))?;
        let items = array(&payload, "data");
        jobs.extend(arbeitnow_leads(items, config));
        if items.is_empty() {
            break;
        }
    }
    Ok(dedupe_jobs(jobs))
}

fn arbeitnow_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
    items
        .iter()
        .map(|job| JobLead {
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
        })
        .collect()
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
    Ok(dedupe_jobs(remotive_leads(array(&payload, "jobs"), config)))
}

fn remotive_leads(items: &[Value], config: &Value) -> Vec<JobLead> {
    items
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
        .collect()
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

    // --- feed parsing ----------------------------------------------------

    #[test]
    fn parses_rss_items() {
        let rss = r#"<rss><channel><item><title>Support Engineer</title><link>https://example/jobs/1</link><description><![CDATA[Help <b>people</b>]]></description><author>Acme</author><location>London</location><pubDate>Mon, 01 Jan 2026</pubDate></item></channel></rss>"#;
        let jobs = parse_feed(rss, "Test").unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].title, "Support Engineer");
        assert_eq!(jobs[0].link, "https://example/jobs/1");
        assert_eq!(jobs[0].description, "Help people");
        assert_eq!(jobs[0].company, "Acme");
        assert_eq!(jobs[0].location, "London");
        assert_eq!(jobs[0].date_posted, "Mon, 01 Jan 2026");
        assert_eq!(jobs[0].source, "Test");
    }

    // Atom carries its URL on a self-closing <link href="..."/>, which is a
    // different quick-xml event from the RSS text node.
    #[test]
    fn parses_atom_entries() {
        let atom = r#"<feed>
            <entry>
                <title>Service Desk Analyst</title>
                <link rel="alternate" href="https://example/jobs/2"/>
                <summary>First half.</summary>
                <content>Second half.</content>
                <updated>2026-01-02</updated>
            </entry>
            <entry>
                <title><![CDATA[Desktop Support]]></title>
                <link href="https://example/jobs/3"/>
                <summary><![CDATA[Windows estate]]></summary>
            </entry>
        </feed>"#;
        let jobs = parse_feed(atom, "Atom").unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].title, "Service Desk Analyst");
        assert_eq!(jobs[0].link, "https://example/jobs/2");
        // Repeated description fields are joined, not overwritten.
        assert_eq!(jobs[0].description, "First half. Second half.");
        assert_eq!(jobs[0].date_posted, "2026-01-02");
        // CDATA feeds the same fields as plain text.
        assert_eq!(jobs[1].title, "Desktop Support");
        assert_eq!(jobs[1].link, "https://example/jobs/3");
        assert_eq!(jobs[1].description, "Windows estate");
    }

    #[test]
    fn feed_entries_need_a_title_and_a_link() {
        let feed = r#"<rss><channel>
            <item><title>No link</title></item>
            <item><link>https://example/jobs/4</link></item>
            <item><title>Keeper</title><link>https://example/jobs/5</link></item>
        </channel></rss>"#;
        let jobs = parse_feed(feed, "Test").unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].title, "Keeper");
    }

    #[test]
    fn feed_ignores_content_outside_an_item() {
        // Text and self-closing tags before any <item> must not be collected.
        let feed = r#"<rss><channel><title>Channel title</title><link href="https://example/feed"/>
            <description><![CDATA[Channel blurb]]></description>
            <item><title>Keeper</title><link>https://example/jobs/6</link></item>
        </channel></rss>"#;
        let jobs = parse_feed(feed, "Test").unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].title, "Keeper");
        assert_eq!(jobs[0].link, "https://example/jobs/6");
        assert_eq!(jobs[0].description, "");
    }

    #[test]
    fn first_link_wins_and_href_is_the_only_attribute_read() {
        let feed = r#"<rss><channel><item>
            <title>Once</title>
            <link>https://example/jobs/first</link>
            <link>https://example/jobs/second</link>
            <link rel="https://example/jobs/wrong"/>
        </item></channel></rss>"#;
        let jobs = parse_feed(feed, "Test").unwrap();
        assert_eq!(jobs[0].link, "https://example/jobs/first");

        // The same rule applies to links arriving as CDATA.
        let cdata = r#"<rss><channel><item>
            <title>Twice</title>
            <link><![CDATA[https://example/jobs/cdata-first]]></link>
            <link><![CDATA[https://example/jobs/cdata-second]]></link>
        </item></channel></rss>"#;
        let jobs = parse_feed(cdata, "Test").unwrap();
        assert_eq!(jobs[0].link, "https://example/jobs/cdata-first");
    }

    #[test]
    fn malformed_feed_is_an_error() {
        assert!(parse_feed("<rss><channel><item></channel>", "Test").is_err());
    }

    #[test]
    fn dedupe_jobs_drops_repeats_and_incomplete_leads() {
        let job = |title: &str, link: &str| JobLead {
            title: title.into(),
            link: link.into(),
            ..JobLead::default()
        };
        let deduped = dedupe_jobs(vec![
            job("First", "https://example/1"),
            job("Duplicate link", "https://example/1"),
            job("", "https://example/2"),
            job("No link", ""),
            job("Second", "https://example/3"),
        ]);
        assert_eq!(
            deduped.iter().map(|j| j.title.as_str()).collect::<Vec<_>>(),
            ["First", "Second"]
        );
    }

    // --- config and JSON helpers -----------------------------------------

    #[test]
    fn cfg_and_display_name_read_string_keys() {
        let config = json!({"name": "wwr", "display_name": "We Work Remotely", "count": 3});
        assert_eq!(cfg(&config, "name"), "wwr");
        assert_eq!(cfg(&config, "missing"), "");
        assert_eq!(cfg(&config, "count"), "");
        assert_eq!(display_name(&config), "We Work Remotely");
        assert_eq!(display_name(&json!({"name": "wwr"})), "wwr");
        assert_eq!(display_name(&json!({})), "");
    }

    #[test]
    fn value_text_cleans_strings_and_stringifies_the_rest() {
        assert_eq!(value_text(Some(&json!("<b>Hi</b>  there"))), "Hi there");
        assert_eq!(value_text(Some(&json!(null))), "");
        assert_eq!(value_text(Some(&json!(42))), "42");
        assert_eq!(value_text(Some(&json!(true))), "true");
        assert_eq!(value_text(None), "");
    }

    #[test]
    fn join_skips_blanks_and_cleans_parts() {
        assert_eq!(
            join(["London".into(), String::new(), "<b>UK</b>".into()]),
            "London UK"
        );
        assert_eq!(join([" ".to_owned()]), "");
        assert_eq!(join(Vec::<String>::new()), "");
    }

    #[test]
    fn array_and_first_tolerate_missing_keys() {
        let value = json!({"items": [1, 2], "name": "", "label": "Second", "wrong": 5});
        assert_eq!(array(&value, "items").len(), 2);
        assert!(array(&value, "missing").is_empty());
        assert!(array(&json!("scalar"), "items").is_empty());
        assert_eq!(first(&value, &["name", "label"]), "Second");
        assert_eq!(first(&value, &["missing"]), "");
        assert_eq!(first(&value, &[]), "");
    }

    #[test]
    fn or_else_substitutes_only_for_an_empty_string() {
        assert_eq!(String::new().or_else("fallback"), "fallback");
        assert_eq!("kept".to_owned().or_else("fallback"), "kept");
    }

    #[test]
    fn required_env_rejects_missing_and_blank_values() {
        // SAFETY: single-threaded test, no worker threads are running.
        unsafe {
            env::set_var("ROLEFORGE_TEST_SET", "value");
            env::set_var("ROLEFORGE_TEST_BLANK", "   ");
            env::remove_var("ROLEFORGE_TEST_MISSING");
        }
        assert_eq!(required_env("ROLEFORGE_TEST_SET").unwrap(), "value");
        assert!(required_env("ROLEFORGE_TEST_BLANK").is_err());
        let error = required_env("ROLEFORGE_TEST_MISSING")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "ROLEFORGE_TEST_MISSING must be set in the environment"
        );
    }

    // --- salary formatting -----------------------------------------------

    #[test]
    fn formats_provider_salary() {
        assert_eq!(
            format_salary(Some(&json!(30_000)), Some(&json!(40_000)), "GBP", "year"),
            "GBP 30000-40000 per year"
        );
        assert_eq!(
            format_salary(Some(&json!("30000.4")), None, "GBP", "year"),
            "GBP 30000 per year"
        );
        assert_eq!(
            format_salary(None, Some(&json!(40_000)), "GBP", "year"),
            "GBP 40000 per year"
        );
        assert_eq!(format_salary(None, None, "GBP", "year"), "");
        assert_eq!(format_salary(Some(&json!("n/a")), None, "GBP", "year"), "");
    }

    #[test]
    fn salary_text_reads_json_ld_shapes() {
        let nested = json!({
            "currency": "GBP",
            "value": {"minValue": 30000, "maxValue": 40000, "unitText": "YEAR"}
        });
        assert_eq!(salary_text(Some(&nested)), "GBP 30000-40000 per YEAR");

        let single = json!({"currency": "USD", "value": {"value": 50000, "unitText": "HOUR"}});
        assert_eq!(salary_text(Some(&single)), "USD 50000 per HOUR");

        // No nested "value" object: min and max are read from the root.
        let flat = json!({"currency": "EUR", "minValue": 10, "maxValue": 20});
        assert_eq!(salary_text(Some(&flat)), "EUR 10-20 per ");

        assert_eq!(salary_text(None), "");
        assert_eq!(salary_text(Some(&json!("free text"))), "");
    }

    // --- HTML scraping ---------------------------------------------------

    #[test]
    fn resolves_anchor_links() {
        let html = r##"
            <a href='/careers/one'>Engineer</a>
            <a href="https://other.example/two">Absolute</a>
            <a href="#skip">Anchor</a>
            <a href="mailto:jobs@example.com">Mail</a>
            <a href="javascript:void(0)">Script</a>
        "##;
        assert_eq!(
            extract_anchor_links(html, "https://example.com/jobs"),
            [
                (
                    "https://example.com/careers/one".to_owned(),
                    "Engineer".to_owned()
                ),
                (
                    "https://other.example/two".to_owned(),
                    "Absolute".to_owned()
                ),
            ]
        );
        assert!(extract_anchor_links(html, "not a url").is_empty());
    }

    #[test]
    fn extracts_json_ld_blocks() {
        let html = r#"
            <script type="application/ld+json">{"@type":"JobPosting","title":"One"}</script>
            <script type="application/ld+json">not json</script>
            <script type="text/javascript">{"@type":"JobPosting"}</script>
        "#;
        let blocks = extract_json_ld(html);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["title"], "One");
        assert!(extract_json_ld("<p>nothing here</p>").is_empty());
    }

    #[test]
    fn visits_nested_job_posting_nodes() {
        let value = json!({
            "@graph": [
                {"@type": "JobPosting", "title": "One"},
                {"@type": ["Thing", "JobPosting"], "title": "Two"},
                {"@type": "Organization", "sub": {"@type": "JobPosting", "title": "Three"}},
                {"@type": "Organization", "title": "Skip"},
                {"@type": ["Thing", "Place"], "title": "Also skip"},
                "a string"
            ]
        });
        let mut nodes = vec![];
        visit_job_nodes(&value, &mut nodes);
        assert_eq!(
            nodes
                .iter()
                .map(|node| node["title"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["One", "Two", "Three"]
        );
    }

    #[test]
    fn builds_a_lead_from_a_json_ld_posting() {
        let posting = json!({
            "title": "IT Support Engineer",
            "url": "https://example.com/jobs/9",
            "hiringOrganization": {"name": "Acme"},
            "jobLocation": [{"address": {
                "addressLocality": "London",
                "addressRegion": "",
                "addressCountry": "UK"
            }}],
            "baseSalary": {"currency": "GBP", "value": {"value": 45000, "unitText": "YEAR"}},
            "description": "<p>Estate support</p>",
            "employmentType": "FULL_TIME",
            "datePosted": "2026-01-01"
        });
        let lead = job_from_json_ld(&posting, "https://fallback", "board", "Fallback Co");
        assert_eq!(lead.title, "IT Support Engineer");
        assert_eq!(lead.link, "https://example.com/jobs/9");
        assert_eq!(lead.source, "board");
        assert_eq!(lead.company, "Acme");
        assert_eq!(lead.location, "London UK");
        assert_eq!(lead.salary, "GBP 45000 per YEAR");
        assert_eq!(lead.description, "Estate support");
        assert_eq!(lead.employment_type, "FULL_TIME");
        assert_eq!(lead.date_posted, "2026-01-01");

        // Everything optional falls back to the caller's values.
        let bare = job_from_json_ld(&json!({}), "https://fallback", "board", "Fallback Co");
        assert_eq!(bare.link, "https://fallback");
        assert_eq!(bare.company, "Fallback Co");
        assert_eq!(bare.location, "");
        assert_eq!(bare.salary, "");

        // An empty organisation name also falls back.
        let anonymous = json!({"hiringOrganization": {"name": ""}, "sameAs": "https://alt"});
        let lead = job_from_json_ld(&anonymous, "https://fallback", "board", "Fallback Co");
        assert_eq!(lead.company, "Fallback Co");
        assert_eq!(lead.link, "https://alt");

        // A single jobLocation object works the same as a one-element array.
        let single = json!({"jobLocation": {"address": {"addressLocality": "Leeds"}}});
        assert_eq!(
            job_from_json_ld(&single, "https://fallback", "board", "Co").location,
            "Leeds"
        );
    }

    #[test]
    fn falls_back_to_the_page_title_and_meta_description() {
        let html = r#"<html><head><title>  Support Engineer &amp; Analyst </title>
            <meta property="og:description" content="Estate support role"></head></html>"#;
        let lead = fallback_html_job(html, "https://example/jobs/1", "board", "Acme").unwrap();
        assert_eq!(lead.title, "Support Engineer & Analyst");
        assert_eq!(lead.link, "https://example/jobs/1");
        assert_eq!(lead.source, "board");
        assert_eq!(lead.company, "Acme");
        assert_eq!(lead.description, "Estate support role");

        // No description meta is fine; no usable title is not a job.
        let titled = fallback_html_job("<title>Only a title</title>", "u", "s", "c").unwrap();
        assert_eq!(titled.description, "");
        assert!(fallback_html_job("<html></html>", "u", "s", "c").is_none());
        assert!(fallback_html_job("<title>  </title>", "u", "s", "c").is_none());
    }

    // --- provider payload mapping ----------------------------------------
    //
    // Each adapter is a URL build, a fetch, and a mapping. Only the mapping is
    // interesting, and only it is reachable without a live request.

    fn board() -> Value {
        json!({"name": "acme", "display_name": "Acme Careers"})
    }

    #[test]
    fn maps_greenhouse_payloads() {
        let leads = greenhouse_leads(
            &[json!({
                "title": "IT Support Engineer",
                "absolute_url": "https://boards.greenhouse.io/acme/jobs/1",
                "location": {"name": "London, UK"},
                "content": "<p>Estate support</p>"
            })],
            &board(),
        );
        assert_eq!(
            leads[0],
            JobLead {
                title: "IT Support Engineer".into(),
                link: "https://boards.greenhouse.io/acme/jobs/1".into(),
                source: "Greenhouse".into(),
                company: "Acme Careers".into(),
                location: "London, UK".into(),
                description: "Estate support".into(),
                ..JobLead::default()
            }
        );
        let bare = &greenhouse_leads(&[json!({})], &board())[0];
        assert_eq!(bare.location, "");
        assert_eq!(bare.source, "Greenhouse");
        assert!(greenhouse_leads(&[], &board()).is_empty());
    }

    #[test]
    fn maps_lever_payloads() {
        let leads = lever_leads(
            &[json!({
                "text": "Service Desk Analyst",
                "hostedUrl": "https://jobs.lever.co/acme/1",
                "applyUrl": "https://jobs.lever.co/acme/1/apply",
                "categories": {"location": "Remote", "commitment": "Full-time"},
                "descriptionPlain": "First.",
                "descriptionBodyPlain": "Second.",
                "additionalPlain": "Third.",
                "salaryDescriptionPlain": "£45,000"
            })],
            &board(),
        );
        assert_eq!(
            leads[0],
            JobLead {
                title: "Service Desk Analyst".into(),
                link: "https://jobs.lever.co/acme/1".into(),
                source: "Lever".into(),
                company: "Acme Careers".into(),
                location: "Remote".into(),
                employment_type: "Full-time".into(),
                description: "First. Second. Third.".into(),
                salary: "£45,000".into(),
                ..JobLead::default()
            }
        );
        // No hostedUrl falls through to applyUrl; no categories is not fatal.
        let fallback = &lever_leads(
            &[json!({"applyUrl": "https://jobs.lever.co/acme/2/apply"})],
            &board(),
        )[0];
        assert_eq!(fallback.link, "https://jobs.lever.co/acme/2/apply");
        assert_eq!(fallback.location, "");
        assert_eq!(fallback.employment_type, "");
    }

    #[test]
    fn maps_ashby_payloads_and_skips_unlisted_jobs() {
        let leads = ashby_leads(
            &[
                json!({
                    "title": "Desktop Support",
                    "jobUrl": "https://jobs.ashbyhq.com/acme/1",
                    "location": "London",
                    "employmentType": "FullTime",
                    "descriptionPlain": "Support.",
                    "department": "IT",
                    "team": "Endpoint",
                    "workplaceType": "Hybrid"
                }),
                json!({"title": "Hidden", "isListed": false}),
                json!({"title": "Listed", "applyUrl": "https://a/2", "isListed": true}),
            ],
            &board(),
        );
        assert_eq!(leads.len(), 2);
        assert_eq!(
            leads[0],
            JobLead {
                title: "Desktop Support".into(),
                link: "https://jobs.ashbyhq.com/acme/1".into(),
                source: "Ashby".into(),
                company: "Acme Careers".into(),
                location: "London".into(),
                employment_type: "FullTime".into(),
                description: "Support. IT Endpoint Hybrid".into(),
                ..JobLead::default()
            }
        );
        assert_eq!(leads[1].link, "https://a/2");
    }

    #[test]
    fn maps_workable_payloads() {
        let leads = workable_leads(
            &[json!({
                "title": "IT Technician",
                "application_url": "https://acme.workable.com/j/1",
                "location": {"location_str": "London, UK", "city": "London", "country": "UK"},
                "description": "<p>Desk work</p>",
                "description_plain": "Desk work plain",
                "department": "IT"
            })],
            &board(),
        );
        assert_eq!(
            leads[0],
            JobLead {
                title: "IT Technician".into(),
                link: "https://acme.workable.com/j/1".into(),
                source: "Workable".into(),
                company: "Acme Careers".into(),
                location: "London, UK London UK".into(),
                description: "Desk work Desk work plain IT".into(),
                ..JobLead::default()
            }
        );
        // Field fallbacks, in order.
        let fallback = &workable_leads(
            &[json!({"full_title": "Fallback title", "shortlink": "https://s/1"})],
            &board(),
        )[0];
        assert_eq!(fallback.title, "Fallback title");
        assert_eq!(fallback.link, "https://s/1");
        assert_eq!(fallback.location, "");
    }

    #[test]
    fn maps_adzuna_payloads_and_picks_the_currency_by_country() {
        let job = json!({
            "title": "Support Analyst",
            "redirect_url": "https://adzuna/1",
            "company": {"display_name": "Acme"},
            "location": {"display_name": "London"},
            "salary_min": 30000,
            "salary_max": 40000,
            "description": "Analyst work",
            "created": "2026-01-01"
        });
        assert_eq!(
            adzuna_leads(std::slice::from_ref(&job), &board(), "gb")[0],
            JobLead {
                title: "Support Analyst".into(),
                link: "https://adzuna/1".into(),
                source: "Acme Careers".into(),
                company: "Acme".into(),
                location: "London".into(),
                salary: "GBP 30000-40000 per year".into(),
                description: "Analyst work".into(),
                date_posted: "2026-01-01".into(),
                ..JobLead::default()
            }
        );
        assert_eq!(
            adzuna_leads(std::slice::from_ref(&job), &board(), "us")[0].salary,
            "USD 30000-40000 per year"
        );
        let bare = &adzuna_leads(&[json!({})], &board(), "gb")[0];
        assert_eq!(bare.company, "");
        assert_eq!(bare.location, "");
        assert_eq!(bare.salary, "");
    }

    #[test]
    fn maps_reed_payloads() {
        let leads = reed_leads(
            &[json!({
                "jobTitle": "Service Desk",
                "jobUrl": "https://reed/1",
                "employerName": "Globex",
                "locationName": "Leeds",
                "minimumSalary": 28000,
                "maximumSalary": 32000,
                "jobDescription": "Desk work",
                "date": "01/01/2026"
            })],
            &board(),
        );
        assert_eq!(
            leads[0],
            JobLead {
                title: "Service Desk".into(),
                link: "https://reed/1".into(),
                source: "Acme Careers".into(),
                company: "Globex".into(),
                location: "Leeds".into(),
                salary: "GBP 28000-32000 per year".into(),
                description: "Desk work".into(),
                date_posted: "01/01/2026".into(),
                employment_type: String::new(),
            }
        );
        assert_eq!(reed_leads(&[json!({})], &board())[0].salary, "");
    }

    #[test]
    fn maps_jooble_payloads() {
        let leads = jooble_leads(
            &[json!({
                "title": "IT Support",
                "link": "https://jooble/1",
                "company": "Initech",
                "location": "London",
                "salary": "£40,000",
                "snippet": "Snippet text",
                "type": "Full-time",
                "updated": "2026-01-01"
            })],
            &board(),
        );
        assert_eq!(
            leads[0],
            JobLead {
                title: "IT Support".into(),
                link: "https://jooble/1".into(),
                source: "Acme Careers".into(),
                company: "Initech".into(),
                location: "London".into(),
                salary: "£40,000".into(),
                description: "Snippet text".into(),
                employment_type: "Full-time".into(),
                date_posted: "2026-01-01".into(),
            }
        );
    }

    #[test]
    fn maps_themuse_payloads() {
        let leads = themuse_leads(
            &[json!({
                "name": "Support Engineer",
                "refs": {"landing_page": "https://themuse/1"},
                "company": {"name": "Acme"},
                "locations": [{"name": "London"}, {"name": "Manchester"}],
                "contents": "<p>Contents</p>",
                "publication_date": "2026-01-01"
            })],
            &board(),
        );
        assert_eq!(
            leads[0],
            JobLead {
                title: "Support Engineer".into(),
                link: "https://themuse/1".into(),
                source: "Acme Careers".into(),
                company: "Acme".into(),
                location: "London, Manchester".into(),
                description: "Contents".into(),
                date_posted: "2026-01-01".into(),
                ..JobLead::default()
            }
        );
        let bare = &themuse_leads(&[json!({})], &board())[0];
        assert_eq!(bare.link, "");
        assert_eq!(bare.company, "");
        assert_eq!(bare.location, "");
    }

    #[test]
    fn maps_arbeitnow_payloads() {
        let leads = arbeitnow_leads(
            &[json!({
                "title": "IT Supporter",
                "url": "https://arbeitnow/1",
                "company_name": "Globex",
                "location": "Berlin",
                "description": "<p>Support</p>",
                "job_types": ["full_time", "permanent"]
            })],
            &board(),
        );
        assert_eq!(
            leads[0],
            JobLead {
                title: "IT Supporter".into(),
                link: "https://arbeitnow/1".into(),
                source: "Acme Careers".into(),
                company: "Globex".into(),
                location: "Berlin".into(),
                description: "Support".into(),
                employment_type: "full_time, permanent".into(),
                ..JobLead::default()
            }
        );
        assert_eq!(
            arbeitnow_leads(&[json!({})], &board())[0].employment_type,
            ""
        );
    }

    #[test]
    fn maps_remotive_payloads() {
        let leads = remotive_leads(
            &[json!({
                "title": "Remote Support",
                "url": "https://remotive/1",
                "company_name": "Acme",
                "candidate_required_location": "Europe",
                "salary": "£40k",
                "description": "<p>Remote</p>",
                "job_type": "full_time",
                "publication_date": "2026-01-01"
            })],
            &board(),
        );
        assert_eq!(
            leads[0],
            JobLead {
                title: "Remote Support".into(),
                link: "https://remotive/1".into(),
                source: "Acme Careers".into(),
                company: "Acme".into(),
                location: "Europe".into(),
                salary: "£40k".into(),
                description: "Remote".into(),
                employment_type: "full_time".into(),
                date_posted: "2026-01-01".into(),
            }
        );
    }
}
