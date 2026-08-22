use crate::{
    Result,
    config::{self, BORDERLINE_MARGIN, MIN_MATCH_SCORE, STATE_DB},
    matching::{Geo, score_job},
    model::string,
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
    let mut all_sources = config::built_in_sources();
    all_sources.extend(config::load_company_boards("company_boards.json")?);
    let geo = Geo::new(&profile);
    let mut matches = vec![];
    let mut borderline = vec![];
    let mut new_rows = vec![];
    let mut reviewed = 0;

    for source in all_sources {
        let name = string(source.get("name"));
        if name.is_empty() {
            continue;
        }
        if !feed_due(state.get(&name), now, feed_interval(&source)) {
            continue;
        }
        let label = or_fallback(string(source.get("display_name")), &name);
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
                    // A linkless lead cannot be deduplicated or applied to.
                    if item.link.is_empty()
                        || links.contains(&item.link)
                        || db.has_reviewed(&fingerprints)?
                    {
                        continue;
                    }
                    let result = score_job(&item, &label, &profile, &search, &timestamp, &geo);
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
    db.save_applications_state(&applications)?;

    let mut alerts = db.alert_state()?;
    let queued = telegram::queue_alerts(&mut alerts, &matches);
    let (sent, delivery_error) = telegram::deliver_alerts(&mut alerts, &timestamp);
    alerts["last_run_utc"] = Value::String(timestamp.clone());
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
    loop {
        match Database::open(STATE_DB).and_then(|db| telegram::process_updates(&db, 25)) {
            Ok(_) => {}
            Err(error) => {
                warn!("Telegram worker: {error}");
                thread::sleep(Duration::from_secs(if telegram::token().is_empty() {
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
