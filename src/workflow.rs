use crate::Result;
use crate::config::{self, BORDERLINE_MARGIN, MIN_MATCH_SCORE, STATE_DB};
use crate::matching::score_job;
use crate::model::string;
use crate::sources;
use crate::storage::{self, Database, FeedState, JobRow};
use crate::telegram;
use crate::text::review_fingerprints;
use chrono::{Timelike, Utc};
use log::{error, info, warn};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn pull() -> Result<()> {
    let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let profile = config::load_resume("resume.json")?;
    let search = config::load_search_config("job_search_config.json")?;
    let mut db = Database::open(STATE_DB)?;
    let existing = db.jobs()?;
    let mut links: HashSet<String> = existing.iter().map(|row| row.lead.link.clone()).collect();
    for row in &existing {
        db.append_reviewed(
            &review_fingerprints(&row.lead.title, &row.lead.description, &row.lead.link),
            50_000,
        )?;
    }

    let mut state = db.feed_state()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    let mut all_sources = config::built_in_sources();
    all_sources.extend(config::load_company_boards("company_boards.json")?);
    let regions = [
        "us",
        "usa",
        "united states",
        "uk",
        "united kingdom",
        "canada",
        "europe",
        "americas",
    ];
    let lockouts: Vec<String> = regions
        .iter()
        .filter(|region| {
            !profile
                .preferred_locations
                .iter()
                .any(|item| item == **region)
        })
        .flat_map(|region| [format!("{region} only"), format!("remote {region}")])
        .collect();
    let mut matches = vec![];
    let mut borderline = vec![];
    let mut new_rows = vec![];
    let mut reviewed = 0;

    for source in all_sources {
        let name = string(source.get("name"));
        if name.is_empty() {
            continue;
        }
        let interval = source
            .get("min_interval_seconds")
            .and_then(Value::as_f64)
            .or_else(|| {
                source
                    .get("min_interval_seconds")
                    .and_then(Value::as_i64)
                    .map(|v| v as f64)
            })
            .unwrap_or(3600.0);
        if state
            .get(&name)
            .is_some_and(|value| now - value.last_checked_at < interval)
        {
            continue;
        }
        let label = {
            let display = string(source.get("display_name"));
            if display.is_empty() {
                name.clone()
            } else {
                display
            }
        };
        match sources::fetch(&source) {
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
                    if item.link.is_empty()
                        || links.contains(&item.link)
                        || db.has_reviewed(&fingerprints)?
                    {
                        continue;
                    }
                    let result = score_job(&item, &label, &profile, &search, &timestamp, &lockouts);
                    db.append_reviewed(&fingerprints, 50_000)?;
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
                            db.upsert_application(&application)?;
                            matches.push(candidate);
                            new_rows.push(JobRow {
                                time: timestamp.clone(),
                                lead: item.clone(),
                            });
                            links.insert(item.link);
                        } else if result.score >= MIN_MATCH_SCORE - BORDERLINE_MARGIN {
                            borderline.push(candidate);
                        }
                    }
                }
            }
            Err(error) => {
                let failures = state
                    .get(&name)
                    .map_or(1, |value| value.consecutive_failures + 1);
                state.insert(
                    name.clone(),
                    FeedState {
                        last_checked_at: state.get(&name).map_or(0.0, |v| v.last_checked_at),
                        consecutive_failures: failures,
                    },
                );
                warn!("skipping {label}: {error}");
                if failures == 10 {
                    matches.push(json!({"time":timestamp,"title":format!("⚠️ Source Failure: {label}"),"description":format!("The job source '{label}' failed 10 times consecutively."),"link":"https://github.com/gregorycarnegie/roleforge","source":"System Monitor","company":"","qualified":true,"score":100,"reasons":["Source failed 10 times consecutively"]}));
                }
            }
        }
    }

    db.append_jobs(&new_rows)?;
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
    storage::write_json(
        "feedback_metrics.json",
        &json!({"generated_at_utc":timestamp,"enabled":search.feedback.get("enabled").and_then(Value::as_bool).unwrap_or(true),"note":"Historical outcomes remain in the compatible applications table."}),
    )?;
    db.save_applications_state(&applications)?;

    let mut alerts = db.alert_state()?;
    let queued = telegram::queue_alerts(&mut alerts, &matches);
    let (sent, delivery_error) = telegram::deliver_alerts(&mut alerts, &timestamp);
    alerts["last_run_utc"] = Value::String(timestamp.clone());
    db.save_alert_state(&alerts)?;
    storage::write_json("alerts_state.json", &alerts)?;
    storage::write_json("feed_state.json", &serde_json::to_value(state.iter().map(|(name,value)| (name.clone(),json!({"last_checked_at":value.last_checked_at,"consecutive_failures":value.consecutive_failures}))).collect::<serde_json::Map<_,_>>())?)?;
    storage::write_json(
        "seen_jobs_state.json",
        &json!({"reviewed_fingerprints":[],"last_run_utc":timestamp,"note":"Fingerprints are stored authoritatively in SQLite."}),
    )?;
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
    if timestamp != current && timestamp.is_some() {
        storage::write_json("desc.json", &Value::Array(rows))?;
        info!("staged latest matched batch");
    } else if timestamp.is_none() && !Path::new("desc.json").exists() {
        storage::write_json("desc.json", &json!([]))?;
    }
    Ok(())
}

pub fn worker() -> Result<()> {
    loop {
        match Database::open(STATE_DB).and_then(|db| telegram::process_updates(&db, 25)) {
            Ok(_) => {}
            Err(error) => {
                warn!("Telegram worker: {error}");
                thread::sleep(Duration::from_secs(if telegram::settings().0.is_empty() {
                    30
                } else {
                    5
                }));
            }
        }
    }
}

pub fn run() -> Result<()> {
    thread::spawn(|| {
        if let Err(error) = worker() {
            error!("Telegram worker stopped: {error}");
        }
    });
    loop {
        if let Err(error) = pull().and_then(|_| export_latest()) {
            error!("RoleForge sweep failed: {error}");
        }
        thread::sleep(Duration::from_secs(60));
    }
}

pub fn digest_due(config: &Value, last_date: &str) -> bool {
    config
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        && Utc::now().hour() >= config.get("hour_utc").and_then(Value::as_u64).unwrap_or(7) as u32
        && Utc::now().date_naive().to_string() != last_date
}
