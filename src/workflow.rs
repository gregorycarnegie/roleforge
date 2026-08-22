use crate::{
    Result,
    config::{self, BORDERLINE_MARGIN, MIN_MATCH_SCORE, STATE_DB},
    matching::{Geo, score_job},
    model::{JobLead, string},
    sources,
    storage::{self, Database, FeedState, JobRow},
    telegram,
    text::{or_fallback, review_fingerprints},
};
use chrono::{Timelike, Utc};
use log::{error, info, warn};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::Path,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn pull() -> Result<()> {
    let mut all_sources = config::built_in_sources();
    all_sources.extend(config::load_company_boards("company_boards.json")?);
    sweep(&all_sources, sources::fetch)
}

/// The sweep itself, with the source list and the fetcher passed in so a test
/// can drive the whole path without a network.
fn sweep(all_sources: &[Value], fetch: impl Fn(&Value) -> Result<Vec<JobLead>>) -> Result<()> {
    let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let profile = config::load_resume("resume.json")?;
    let search = config::load_search_config("job_search_config.json")?;
    let mut db = Database::open(STATE_DB)?;
    let existing = db.jobs()?;
    let mut links: HashSet<String> = existing.iter().map(|row| row.lead.link.clone()).collect();
    // Backfill for a database carried over from the Python bot, whose jobs
    // predate fingerprinting. One transaction, not one per stored job.
    let backfill: Vec<String> = existing
        .iter()
        .flat_map(|row| review_fingerprints(&row.lead.title, &row.lead.description, &row.lead.link))
        .collect();
    db.append_reviewed(&backfill, 50_000)?;

    let mut state = db.feed_state()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    let geo = Geo::new(&profile);
    let mut matches = vec![];
    let mut borderline = vec![];
    let mut new_rows = vec![];
    let mut new_applications = vec![];
    // Held until the batch commits: a fingerprint written ahead of the job it
    // came from would hide that job for good on the next run.
    let mut pending_reviewed: Vec<String> = vec![];
    let mut seen_reviewed: HashSet<String> = HashSet::new();
    let mut reviewed = 0;

    for source in all_sources {
        let name = string(source.get("name"));
        if name.is_empty() {
            continue;
        }
        if !feed_due(state.get(&name), now, feed_interval(source))
            || !sources::is_configured(source)
        {
            continue;
        }
        let label = or_fallback(string(source.get("display_name")), &name);
        match fetch(source) {
            Ok(items) => {
                state.insert(
                    name,
                    FeedState {
                        last_checked_at: now,
                        consecutive_failures: 0,
                    },
                );
                for item in items {
                    let fingerprints =
                        review_fingerprints(&item.title, &item.description, &item.link);
                    // A linkless lead cannot be deduplicated or applied to.
                    if item.link.is_empty()
                        || links.contains(&item.link)
                        || fingerprints.iter().any(|fp| seen_reviewed.contains(fp))
                        || db.has_reviewed(&fingerprints)?
                    {
                        continue;
                    }
                    let result = score_job(&item, &label, &profile, &search, &timestamp, &geo);
                    seen_reviewed.extend(fingerprints.iter().cloned());
                    pending_reviewed.extend(fingerprints.iter().cloned());
                    reviewed += 1;
                    if let Some(candidate) = result.candidate.clone() {
                        if result.qualified {
                            let mut application = candidate.clone();
                            application["status"] = Value::String("new".into());
                            application["first_seen_utc"] = Value::String(timestamp.clone());
                            application["last_seen_utc"] = Value::String(timestamp.clone());
                            application["review_fingerprints"] =
                                serde_json::to_value(&fingerprints)?;
                            application["links"] = json!([item.link]);
                            new_applications.push(application);
                            matches.push(candidate);
                            new_rows.push(JobRow {
                                time: timestamp.clone(),
                                lead: item.clone(),
                            });
                            links.insert(item.link);
                        // Just short of the bar: worth recording for review,
                        // not worth alerting on.
                        } else if result.score >= MIN_MATCH_SCORE - BORDERLINE_MARGIN {
                            borderline.push(candidate);
                        }
                    }
                }
            }
            Err(error) => {
                let failed = after_failure(state.get(&name));
                warn!("skipping {label}: {error}");
                if let Some(alert) = source_failure_alert(&label, &timestamp, &failed) {
                    matches.push(alert);
                }
                state.insert(name.clone(), failed);
            }
        }
    }

    // Order is the durability contract. The alert queue is made durable first,
    // then the batch commits the jobs, applications and their fingerprints
    // together. A crash anywhere before that last commit replays the whole
    // batch, and queue_alerts deduplicates what is already pending.
    let mut alerts = db.alert_state()?;
    let queued = telegram::queue_alerts(&mut alerts, &matches);
    alerts["last_run_utc"] = Value::String(timestamp.clone());
    db.save_alert_state(&alerts)?;
    db.commit_batch(&new_rows, &new_applications, &pending_reviewed, 50_000)?;
    db.save_feed_state(&state)?;
    let all_jobs = db.jobs()?;
    storage::export_csv("jobs.csv", &all_jobs)?;
    storage::write_json(
        "matches.json",
        &json!({"run_time_utc":timestamp,"matches":matches}),
    )?;
    storage::write_json(
        "borderline_matches.json",
        &json!({"generated_at_utc":timestamp,"items":borderline.into_iter().take(10).collect::<Vec<_>>()}),
    )?;

    let mut applications = db.applications_state()?;
    applications["last_updated_utc"] = Value::String(timestamp.clone());
    let digest = build_digest(&applications, &search.daily_digest, &timestamp);
    maybe_send_digest(
        &db,
        &mut applications,
        &digest,
        &search.daily_digest,
        &timestamp,
    );
    storage::write_json("applications.json", &applications)?;
    storage::write_json("daily_digest.json", &digest)?;
    storage::write_json(
        "application_briefs.json",
        &json!({"generated_at_utc":timestamp,"items":digest["items"].as_array().cloned().unwrap_or_default().into_iter().filter(|item| item.get("application_ready") == Some(&Value::Bool(true))).take(10).collect::<Vec<_>>()}),
    )?;
    db.save_applications_state(&applications)?;

    let (sent, delivery_error) = telegram::deliver_alerts(&mut db, &mut alerts, &timestamp);
    db.save_alert_state(&alerts)?;
    storage::write_json("alerts_state.json", &alerts)?;
    storage::write_json("feed_state.json", &serde_json::to_value(state.iter().map(|(name,value)| (name.clone(),json!({"last_checked_at":value.last_checked_at,"consecutive_failures":value.consecutive_failures}))).collect::<serde_json::Map<_,_>>())?)?;
    if !delivery_error.is_empty() {
        error!("{delivery_error}");
    }
    info!(
        "found {} new matches, reviewed {reviewed} new items, queued {queued} alerts, sent {sent}",
        new_rows.len()
    );
    Ok(())
}

fn build_digest(applications: &Value, config: &Value, timestamp: &str) -> Value {
    let max_items = config.get("max_items").and_then(Value::as_u64).unwrap_or(8) as usize;
    let allowed: HashSet<&str> = config
        .get("include_statuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut items: Vec<Value> = applications["applications"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|item| {
            allowed.is_empty()
                || allowed.contains(item.get("status").and_then(Value::as_str).unwrap_or("new"))
        })
        .collect();
    items.sort_by_key(|item| {
        std::cmp::Reverse((
            item.get("shortlisted") == Some(&Value::Bool(true)),
            item.get("score")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
        ))
    });
    items.truncate(max_items);
    json!({"generated_at_utc":timestamp,"items":items,"count":items.len()})
}

fn maybe_send_digest(
    db: &Database,
    applications: &mut Value,
    digest: &Value,
    config: &Value,
    timestamp: &str,
) {
    let last_date = applications
        .get("last_digest_date_utc")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !digest_due(config, last_date) {
        return;
    }
    let page_size = config
        .get("page_size")
        .and_then(Value::as_u64)
        .unwrap_or(4)
        .clamp(1, 10) as usize;
    let items = digest["items"].as_array().cloned().unwrap_or_default();
    let mut pages = vec![];
    if items.is_empty() {
        pages.push(
            "RoleForge Daily Digest\n\nNo matching roles are currently awaiting review.".into(),
        );
    }
    for chunk in items.chunks(page_size) {
        let mut lines = vec!["RoleForge Daily Digest".to_owned(), String::new()];
        for (index, item) in chunk.iter().enumerate() {
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Untitled role");
            let company = item
                .get("company")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let score = item
                .get("score")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let link = item.get("link").and_then(Value::as_str).unwrap_or_default();
            lines.push(format!(
                "{}. {title}{} — score {score}\n{link}",
                index + 1,
                if company.is_empty() {
                    String::new()
                } else {
                    format!(" at {company}")
                }
            ));
        }
        pages.push(lines.join("\n"));
    }
    let session = timestamp
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .rev()
        .take(24)
        .collect::<String>();
    match db
        .save_digest_session(&session, timestamp, &pages)
        .and_then(|_| {
            telegram::send_message(
                &pages[0],
                telegram::digest_keyboard(&session, 0, pages.len()),
            )
        }) {
        Ok(_) => {
            applications["last_digest_utc"] = Value::String(timestamp.into());
            applications["last_digest_date_utc"] =
                Value::String(Utc::now().date_naive().to_string());
            applications["last_digest_error"] = Value::String(String::new());
        }
        Err(error) => applications["last_digest_error"] = Value::String(error.to_string()),
    }
}

pub fn export_latest() -> Result<()> {
    let db = Database::open(STATE_DB)?;
    let (timestamp, rows) = db.latest_batch()?;
    let current = fs::read_to_string("desc.json")
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .as_array()?
                .first()?
                .get("time")?
                .as_str()
                .map(str::to_owned)
        });
    match export_action(timestamp.as_deref(), current.as_deref(), || {
        Path::new("desc.json").exists()
    }) {
        Export::Batch => {
            storage::write_json("desc.json", &Value::Array(rows))?;
            info!("staged latest matched batch");
        }
        Export::Empty => storage::write_json("desc.json", &json!([]))?,
        Export::Skip => {}
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Export {
    /// The newest batch has not been staged yet.
    Batch,
    /// Nothing has ever been collected and there is no file to leave alone.
    Empty,
    Skip,
}

fn export_action(latest: Option<&str>, staged: Option<&str>, exists: impl Fn() -> bool) -> Export {
    match latest {
        Some(latest) if Some(latest) != staged => Export::Batch,
        Some(_) => Export::Skip,
        None if !exists() => Export::Empty,
        None => Export::Skip,
    }
}

pub fn worker() -> Result<()> {
    // Telegram is optional. Without a token there is nothing to poll, and a
    // retry loop would only reprint that for the life of the process.
    if telegram::token().is_empty() {
        return Err("TELEGRAM_BOT_TOKEN not configured".into());
    }
    loop {
        if let Err(error) =
            Database::open(STATE_DB).and_then(|db| telegram::process_updates(&db, 25))
        {
            warn!("Telegram worker: {error}");
            thread::sleep(Duration::from_secs(5));
        }
    }
}

pub fn run() -> Result<()> {
    thread::spawn(|| {
        if let Err(error) = worker() {
            info!("Telegram worker not running: {error}");
        }
    });
    loop {
        if let Err(error) = pull().and_then(|_| export_latest()) {
            error!("RoleForge sweep failed: {error}");
        }
        thread::sleep(Duration::from_secs(60));
    }
}

/// Sources may state their polling interval as either JSON number type.
fn feed_interval(source: &Value) -> f64 {
    source
        .get("min_interval_seconds")
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_i64().map(|seconds| seconds as f64))
        })
        .unwrap_or(3600.0)
}

/// A feed never polled before is always due.
fn feed_due(state: Option<&FeedState>, now: f64, interval: f64) -> bool {
    state.is_none_or(|value| now - value.last_checked_at >= interval)
}

fn after_failure(previous: Option<&FeedState>) -> FeedState {
    FeedState {
        last_checked_at: previous.map_or(0.0, |value| value.last_checked_at),
        consecutive_failures: previous.map_or(1, |value| value.consecutive_failures + 1),
    }
}

/// Alert once, on the tenth consecutive failure, so a dead source is noticed
/// without alerting on every poll afterwards.
fn source_failure_alert(label: &str, timestamp: &str, state: &FeedState) -> Option<Value> {
    (state.consecutive_failures == 10).then(|| {
        json!({"time":timestamp,"title":format!("⚠️ Source Failure: {label}"),"description":format!("The job source '{label}' failed 10 times consecutively."),"link":"https://github.com/gregorycarnegie/roleforge","source":"System Monitor","company":"","qualified":true,"score":100,"reasons":["Source failed 10 times consecutively"]})
    })
}

pub fn digest_due(config: &Value, last_date: &str) -> bool {
    config
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        && Utc::now().hour() >= config.get("hour_utc").and_then(Value::as_u64).unwrap_or(7) as u32
        && Utc::now().date_naive().to_string() != last_date
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    const RESUME: &str = r#"{
        "personal_info": {
            "location": {"city": "London", "country": "United Kingdom"},
            "target_roles": ["IT Support Engineer"],
            "preferences": {
                "remote": true, "hybrid": true, "onsite": true,
                "minimum_salary_gbp": 40000
            }
        },
        "technical_skills": {
            "skills": ["Intune", "Active Directory"],
            "competencies": ["stakeholder management"]
        },
        "experience": [
            {"title": "IT Support Engineer", "company": "Initech",
             "responsibilities": ["Rolled out Intune to 900 devices"]}
        ]
    }"#;

    // hour_utc 0 keeps the digest deterministic: it is always due, so the run
    // exercises that path on every sweep rather than only after 07:00 UTC.
    const SEARCH: &str = r#"{
        "company_whitelist": ["acme"],
        "daily_digest": {"enabled": true, "hour_utc": 0}
    }"#;

    fn matching_lead() -> JobLead {
        JobLead {
            title: "IT Support Engineer".into(),
            link: "https://example.com/jobs/1".into(),
            company: "Acme Ltd".into(),
            location: "London".into(),
            salary: "£45,000 - £55,000 per year".into(),
            employment_type: "Permanent".into(),
            description: "Hybrid in London. Intune and Active Directory estate,                           ticket management and stakeholder management."
                .into(),
            ..JobLead::default()
        }
    }

    /// The orchestration path, end to end, with the network replaced by a
    /// closure: collection, scoring, the batch commit, every export, the
    /// digest, and the alert outbox surviving a failed delivery.
    #[test]
    fn a_sweep_persists_its_batch_and_replays_the_alert_it_could_not_deliver() {
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // SAFETY: the lock keeps the other environment tests out, and no
        // worker threads run here. Unset, every Telegram call fails before it
        // reaches the network, which is the delivery failure under test.
        unsafe {
            env::remove_var("TELEGRAM_BOT_TOKEN");
            env::remove_var("TELEGRAM_CHAT_ID");
        }
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("resume.json"), RESUME).unwrap();
        fs::write(dir.path().join("job_search_config.json"), SEARCH).unwrap();
        // Interval zero so the second sweep polls again instead of waiting an
        // hour: the deduplication under test is the fingerprint, not the clock.
        let feed = json!({"name": "fake", "display_name": "Fake Board", "min_interval_seconds": 0});
        let broken = json!({"name": "dead", "min_interval_seconds": 0});

        let lead = matching_lead();
        let fingerprints = review_fingerprints(&lead.title, &lead.description, &lead.link);

        let previous = env::current_dir().unwrap();
        env::set_current_dir(dir.path()).unwrap();
        let first = sweep(std::slice::from_ref(&feed), |_| Ok(vec![matching_lead()]));
        // Read between the sweeps: the second one's backfill re-derives a
        // fingerprint for every stored job, and would mask a first sweep that
        // never recorded one.
        let reviewed_after_first =
            Database::open(dir.path().join(STATE_DB)).and_then(|db| db.has_reviewed(&fingerprints));
        // The same posting again, alongside a source that is down.
        let second = sweep(&[feed, broken], |source| {
            match string(source.get("name")).as_str() {
                "dead" => Err("connection refused".into()),
                _ => Ok(vec![matching_lead()]),
            }
        });
        // The other entry point, against the database the sweep just wrote.
        let staged = export_latest();
        env::set_current_dir(previous).unwrap();
        first.unwrap();
        assert!(reviewed_after_first.unwrap());
        second.unwrap();
        staged.unwrap();

        let db = Database::open(dir.path().join(STATE_DB)).unwrap();
        // Seen twice, stored once.
        let jobs = db.jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].lead.title, "IT Support Engineer");

        let applications = db.applications_state().unwrap();
        let stored = applications["applications"].as_array().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["status"], "new");
        assert_eq!(stored[0]["company"], "Acme Ltd");
        assert_eq!(stored[0]["source"], "Fake Board");
        assert!(stored[0]["score"].as_i64().unwrap() >= MIN_MATCH_SCORE);
        // The digest ran and recorded why it could not be sent.
        assert!(
            applications["last_digest_error"]
                .as_str()
                .unwrap()
                .contains("TELEGRAM_BOT_TOKEN")
        );

        // The undeliverable alert is still queued, exactly once, after a
        // second sweep that found nothing new.
        let alerts = db.alert_state().unwrap();
        assert_eq!(alerts["pending_alerts"].as_array().unwrap().len(), 1);
        assert_eq!(alerts["pending_alerts"][0]["link"], jobs[0].lead.link);
        assert_eq!(alerts["alerted_links"], json!([]));
        assert!(
            alerts["last_delivery_error"]
                .as_str()
                .unwrap()
                .contains("TELEGRAM_BOT_TOKEN")
        );

        // The dead source is counted, not fatal, and never marked as polled.
        let state = db.feed_state().unwrap();
        assert_eq!(state["dead"].consecutive_failures, 1);
        assert_eq!(state["dead"].last_checked_at, 0.0);
        assert_eq!(state["fake"].consecutive_failures, 0);

        let csv = fs::read_to_string(dir.path().join("jobs.csv")).unwrap();
        assert_eq!(csv.lines().count(), 2);
        assert!(csv.contains("https://example.com/jobs/1"), "{csv}");
        for name in [
            "matches.json",
            "borderline_matches.json",
            "applications.json",
            "daily_digest.json",
            "application_briefs.json",
            "alerts_state.json",
            "feed_state.json",
        ] {
            assert!(dir.path().join(name).exists(), "{name} was not written");
        }
        // The second sweep found nothing, so the newest matches file is empty.
        let matches: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("matches.json")).unwrap())
                .unwrap();
        assert_eq!(matches["matches"], json!([]));

        // export_latest stages the newest stored batch for the OpenClaw prompt.
        let staged: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join("desc.json")).unwrap())
                .unwrap();
        assert_eq!(staged.as_array().unwrap().len(), 1);
        assert_eq!(staged[0]["link"], "https://example.com/jobs/1");
        assert_eq!(staged[0]["title"], "IT Support Engineer");
    }

    fn application(link: &str, status: &str, score: i64, shortlisted: bool) -> Value {
        json!({"link": link, "status": status, "score": score, "shortlisted": shortlisted})
    }

    #[test]
    fn digest_ranks_shortlisted_first_then_score() {
        let applications = json!({
            "applications": [
                application("a", "new", 90, false),
                application("b", "new", 40, true),
                application("c", "new", 95, false),
                application("d", "new", 60, true),
            ]
        });
        let digest = build_digest(&applications, &json!({}), "2026-01-01T00:00:00Z");
        assert_eq!(digest["generated_at_utc"], "2026-01-01T00:00:00Z");
        assert_eq!(digest["count"], 4);
        assert_eq!(
            digest["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["link"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["d", "b", "c", "a"]
        );
    }

    #[test]
    fn digest_filters_by_status_and_caps_the_item_count() {
        let applications = json!({
            "applications": [
                application("a", "new", 90, false),
                application("b", "archived", 95, false),
                application("c", "applied", 80, false),
                json!({"link": "d", "score": 70}),
            ]
        });
        let filtered = build_digest(
            &applications,
            &json!({"include_statuses": ["new", "applied"]}),
            "t",
        );
        // A record with no status counts as "new".
        assert_eq!(
            filtered["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["link"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "c", "d"]
        );

        let capped = build_digest(&applications, &json!({"max_items": 2}), "t");
        assert_eq!(capped["count"], 2);
        assert_eq!(capped["items"][0]["link"], "b");

        // An empty status list means no filtering at all.
        let unfiltered = build_digest(&applications, &json!({"include_statuses": []}), "t");
        assert_eq!(unfiltered["count"], 4);

        // The default cap is eight.
        let many = json!({
            "applications": (0..12).map(|i| application(&i.to_string(), "new", i, false))
                .collect::<Vec<_>>()
        });
        assert_eq!(build_digest(&many, &json!({}), "t")["count"], 8);
    }

    #[test]
    fn digest_of_nothing_is_empty() {
        let digest = build_digest(&json!({}), &json!({}), "t");
        assert_eq!(digest["count"], 0);
        assert_eq!(digest["items"], json!([]));
    }

    #[test]
    fn digest_due_needs_enabled_the_hour_and_a_new_day() {
        let today = Utc::now().date_naive().to_string();

        // hour_utc 0 is always reached; hour_utc 24 never is.
        assert!(digest_due(&json!({"hour_utc": 0}), ""));
        assert!(!digest_due(&json!({"hour_utc": 24}), ""));
        assert!(!digest_due(&json!({"enabled": false, "hour_utc": 0}), ""));
        assert!(digest_due(&json!({"enabled": true, "hour_utc": 0}), ""));
        // Already sent today.
        assert!(!digest_due(&json!({"hour_utc": 0}), &today));
        // Defaults: enabled, and the 07:00 hour, so only the date is asserted.
        assert!(!digest_due(&json!({}), &today));
    }

    #[test]
    fn feed_interval_accepts_either_json_number_type() {
        assert_eq!(feed_interval(&json!({"min_interval_seconds": 900})), 900.0);
        assert_eq!(
            feed_interval(&json!({"min_interval_seconds": 900.5})),
            900.5
        );
        assert_eq!(
            feed_interval(&json!({"min_interval_seconds": "900"})),
            3600.0
        );
        assert_eq!(feed_interval(&json!({})), 3600.0);
    }

    #[test]
    fn feeds_are_due_once_the_interval_has_fully_elapsed() {
        let checked = |seconds| FeedState {
            last_checked_at: seconds,
            consecutive_failures: 0,
        };
        // Never polled.
        assert!(feed_due(None, 1_000.0, 900.0));
        // Exactly at the interval counts as due.
        assert!(feed_due(Some(&checked(100.0)), 1_000.0, 900.0));
        assert!(feed_due(Some(&checked(0.0)), 1_000.0, 900.0));
        assert!(!feed_due(Some(&checked(101.0)), 1_000.0, 900.0));
        assert!(!feed_due(Some(&checked(1_000.0)), 1_000.0, 900.0));
    }

    #[test]
    fn failures_accumulate_and_alert_exactly_once() {
        let first = after_failure(None);
        assert_eq!(first.consecutive_failures, 1);
        assert_eq!(first.last_checked_at, 0.0);

        // A failure does not count as a successful poll, so the timestamp holds.
        let previous = FeedState {
            last_checked_at: 1_234.5,
            consecutive_failures: 8,
        };
        let next = after_failure(Some(&previous));
        assert_eq!(next.consecutive_failures, 9);
        assert_eq!(next.last_checked_at, 1_234.5);

        assert!(source_failure_alert("wwr", "t", &next).is_none());
        let tenth = after_failure(Some(&next));
        assert_eq!(tenth.consecutive_failures, 10);
        let alert = source_failure_alert("wwr", "t", &tenth).unwrap();
        assert_eq!(alert["title"], "⚠️ Source Failure: wwr");
        assert_eq!(alert["time"], "t");
        assert_eq!(alert["score"], 100);
        assert_eq!(alert["qualified"], true);
        // Silent again afterwards.
        assert!(source_failure_alert("wwr", "t", &after_failure(Some(&tenth))).is_none());
    }

    #[test]
    fn export_writes_only_a_batch_that_is_not_already_staged() {
        let never = || panic!("existence must not be checked when a batch exists");
        assert_eq!(export_action(Some("t2"), Some("t1"), never), Export::Batch);
        assert_eq!(export_action(Some("t1"), None, never), Export::Batch);
        assert_eq!(export_action(Some("t1"), Some("t1"), never), Export::Skip);
        // Nothing collected yet: seed an empty file, but never clobber one.
        assert_eq!(export_action(None, None, || false), Export::Empty);
        assert_eq!(export_action(None, None, || true), Export::Skip);
        assert_eq!(export_action(None, Some("t1"), || true), Export::Skip);
    }
}
