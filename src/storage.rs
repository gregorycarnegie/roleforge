use crate::Result;
use crate::model::JobLead;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct JobRow {
    pub time: String,
    pub lead: JobLead,
}

#[derive(Clone, Debug, Default)]
pub struct FeedState {
    pub last_checked_at: f64,
    pub consecutive_failures: i64,
}

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        initialize(&connection)?;
        Ok(Self { connection })
    }

    pub fn jobs(&self) -> Result<Vec<JobRow>> {
        let mut statement = self.connection.prepare("SELECT time,title,description,link,company,location,salary,source,employment_type,date_posted FROM jobs ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok(JobRow {
                time: row.get(0)?,
                lead: JobLead {
                    title: row.get(1)?,
                    description: row.get(2)?,
                    link: row.get(3)?,
                    company: row.get(4)?,
                    location: row.get(5)?,
                    salary: row.get(6)?,
                    source: row.get(7)?,
                    employment_type: row.get(8)?,
                    date_posted: row.get(9)?,
                },
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn latest_batch(&self) -> Result<(Option<String>, Vec<Value>)> {
        let timestamp: Option<String> = self
            .connection
            .query_row(
                "SELECT time FROM jobs ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(timestamp) = timestamp else {
            return Ok((None, vec![]));
        };
        let mut statement = self
            .connection
            .prepare("SELECT time,title,description,link FROM jobs WHERE time=? ORDER BY id")?;
        let rows = statement.query_map([&timestamp], |row| Ok(json!({"time":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"description":row.get::<_,String>(2)?,"link":row.get::<_,String>(3)?})))?;
        Ok((
            Some(timestamp),
            rows.collect::<std::result::Result<Vec<_>, _>>()?,
        ))
    }

    pub fn append_jobs(&mut self, rows: &[JobRow]) -> Result<()> {
        let tx = self.connection.transaction()?;
        for row in rows {
            tx.execute("INSERT OR IGNORE INTO jobs(time,title,description,link,company,location,salary,source,employment_type,date_posted) VALUES(?,?,?,?,?,?,?,?,?,?)",
                params![row.time,row.lead.title,row.lead.description,row.lead.link,row.lead.company,row.lead.location,row.lead.salary,row.lead.source,row.lead.employment_type,row.lead.date_posted])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn feed_state(&self) -> Result<HashMap<String, FeedState>> {
        let mut statement = self.connection.prepare(
            "SELECT name,last_checked_at,COALESCE(consecutive_failures,0) FROM feed_state",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get(0)?,
                FeedState {
                    last_checked_at: row.get(1)?,
                    consecutive_failures: row.get(2)?,
                },
            ))
        })?;
        Ok(rows.collect::<std::result::Result<HashMap<_, _>, _>>()?)
    }

    pub fn save_feed_state(&mut self, state: &HashMap<String, FeedState>) -> Result<()> {
        let tx = self.connection.transaction()?;
        for (name, value) in state {
            tx.execute("INSERT INTO feed_state(name,last_checked_at,consecutive_failures) VALUES(?,?,?) ON CONFLICT(name) DO UPDATE SET last_checked_at=excluded.last_checked_at,consecutive_failures=excluded.consecutive_failures", params![name,value.last_checked_at,value.consecutive_failures])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn has_reviewed(&self, fingerprints: &[String]) -> Result<bool> {
        for fingerprint in fingerprints {
            if self
                .connection
                .query_row(
                    "SELECT 1 FROM reviewed_fingerprints WHERE fingerprint=? LIMIT 1",
                    [fingerprint],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn append_reviewed(&mut self, fingerprints: &[String], max_items: usize) -> Result<()> {
        let tx = self.connection.transaction()?;
        let mut position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position),-1)+1 FROM reviewed_fingerprints",
            [],
            |row| row.get(0),
        )?;
        for fingerprint in fingerprints {
            if tx.execute(
                "INSERT OR IGNORE INTO reviewed_fingerprints(position,fingerprint) VALUES(?,?)",
                params![position, fingerprint],
            )? > 0
            {
                position += 1;
            }
        }
        trim_table(&tx, "reviewed_fingerprints", max_items)?;
        tx.commit()?;
        Ok(())
    }

    pub fn alert_state(&self) -> Result<Value> {
        let alerted = strings_from_table(
            &self.connection,
            "SELECT link FROM alerted_links ORDER BY position",
        )?;
        let pending = json_from_table(
            &self.connection,
            "SELECT payload_json FROM pending_alerts ORDER BY position",
        )?;
        let metadata = metadata(&self.connection, "alerts")?;
        Ok(
            json!({"alerted_links":alerted,"pending_alerts":pending,"last_run_utc":metadata.get("last_run_utc").cloned().unwrap_or_default(),"last_delivery_utc":metadata.get("last_delivery_utc").cloned().unwrap_or_default(),"last_delivery_error":metadata.get("last_delivery_error").cloned().unwrap_or_default()}),
        )
    }

    pub fn save_alert_state(&mut self, state: &Value) -> Result<()> {
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM alerted_links", [])?;
        for (position, link) in state["alerted_links"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .enumerate()
        {
            tx.execute(
                "INSERT OR IGNORE INTO alerted_links(position,link) VALUES(?,?)",
                params![position as i64, link],
            )?;
        }
        tx.execute("DELETE FROM pending_alerts", [])?;
        for (position, alert) in state["pending_alerts"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let link = alert
                .get("link")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !link.is_empty() {
                tx.execute("INSERT OR REPLACE INTO pending_alerts(position,link,payload_json) VALUES(?,?,?)", params![position as i64,link,serde_json::to_string(alert)?])?;
            }
        }
        save_metadata(
            &tx,
            "alerts",
            state,
            &["last_run_utc", "last_delivery_utc", "last_delivery_error"],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn applications_state(&self) -> Result<Value> {
        let applications = json_from_table(
            &self.connection,
            "SELECT payload_json FROM applications ORDER BY position",
        )?;
        let data = metadata(&self.connection, "applications")?;
        Ok(
            json!({"applications":applications,"last_updated_utc":get(&data,"last_updated_utc"),"last_digest_utc":get(&data,"last_digest_utc"),"last_digest_date_utc":get(&data,"last_digest_date_utc"),"last_digest_error":get(&data,"last_digest_error"),"last_feedback_utc":get(&data,"last_feedback_utc"),"last_cleanup_utc":get(&data,"last_cleanup_utc")}),
        )
    }

    pub fn save_applications_state(&mut self, state: &Value) -> Result<()> {
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM applications", [])?;
        tx.execute("DELETE FROM application_links", [])?;
        tx.execute("DELETE FROM application_fingerprints", [])?;
        for (position, application) in state["applications"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            save_application(&tx, application, position as i64)?;
        }
        save_metadata(
            &tx,
            "applications",
            state,
            &[
                "last_updated_utc",
                "last_digest_utc",
                "last_digest_date_utc",
                "last_digest_error",
                "last_feedback_utc",
                "last_cleanup_utc",
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_application(&mut self, record: &Value) -> Result<bool> {
        let link = record
            .get("link")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if link.is_empty() {
            return Ok(false);
        }
        let previous: Option<String> = self
            .connection
            .query_row(
                "SELECT payload_json FROM applications WHERE link=?",
                [link],
                |row| row.get(0),
            )
            .optional()?;
        let position = self.connection.query_row(
            "SELECT COALESCE(MAX(position),-1)+1 FROM applications",
            [],
            |row| row.get(0),
        )?;
        let tx = self.connection.transaction()?;
        save_application(&tx, record, position)?;
        tx.commit()?;
        Ok(previous.is_none())
    }

    pub fn telegram_offset(&self) -> Result<i64> {
        Ok(metadata(&self.connection, "telegram")?
            .get("update_offset")
            .and_then(|v| v.parse().ok())
            .unwrap_or_default())
    }
    pub fn save_telegram_offset(&self, value: i64) -> Result<()> {
        self.connection.execute("INSERT INTO state_metadata(scope,key,value) VALUES('telegram','update_offset',?) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value", [value.to_string()])?;
        Ok(())
    }

    pub fn save_digest_session(
        &self,
        session: &str,
        created_at: &str,
        pages: &[String],
    ) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO telegram_digest_sessions(session_id,created_at,pages_json) VALUES(?,?,?)",
            params![session, created_at, serde_json::to_string(pages)?],
        )?;
        self.connection.execute(
            "DELETE FROM telegram_digest_sessions WHERE session_id NOT IN (SELECT session_id FROM telegram_digest_sessions ORDER BY created_at DESC LIMIT 20)",
            [],
        )?;
        Ok(())
    }

    pub fn digest_session(&self, session: &str) -> Result<Option<Vec<String>>> {
        let value: Option<String> = self
            .connection
            .query_row(
                "SELECT pages_json FROM telegram_digest_sessions WHERE session_id=?",
                [session],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.and_then(|value| serde_json::from_str(&value).ok()))
    }
}

fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch(r#"
        CREATE TABLE IF NOT EXISTS migrations(id INTEGER PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS jobs(id INTEGER PRIMARY KEY AUTOINCREMENT,time TEXT NOT NULL,title TEXT NOT NULL,description TEXT NOT NULL,link TEXT NOT NULL UNIQUE,company TEXT DEFAULT '',location TEXT DEFAULT '',salary TEXT DEFAULT '',source TEXT DEFAULT '',employment_type TEXT DEFAULT '',date_posted TEXT DEFAULT '');
        CREATE INDEX IF NOT EXISTS idx_jobs_time ON jobs(time);
        CREATE TABLE IF NOT EXISTS feed_state(name TEXT PRIMARY KEY,last_checked_at REAL NOT NULL,consecutive_failures INTEGER DEFAULT 0);
        CREATE TABLE IF NOT EXISTS state_metadata(scope TEXT NOT NULL,key TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(scope,key));
        CREATE TABLE IF NOT EXISTS reviewed_fingerprints(position INTEGER NOT NULL,fingerprint TEXT PRIMARY KEY);
        CREATE INDEX IF NOT EXISTS idx_reviewed_fingerprints_position ON reviewed_fingerprints(position);
        CREATE TABLE IF NOT EXISTS alerted_links(position INTEGER NOT NULL,link TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS pending_alerts(position INTEGER NOT NULL,link TEXT PRIMARY KEY,payload_json TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS applications(position INTEGER NOT NULL,link TEXT PRIMARY KEY,payload_json TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS application_links(link_value TEXT PRIMARY KEY,application_link TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS application_fingerprints(fingerprint TEXT PRIMARY KEY,application_link TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS telegram_digest_sessions(session_id TEXT PRIMARY KEY,created_at TEXT NOT NULL,pages_json TEXT NOT NULL);
        INSERT OR IGNORE INTO migrations(id) VALUES(1),(2),(3);
    "#)?;
    // Older databases may have migration rows without all columns after manual repairs.
    let columns = table_columns(connection, "jobs")?;
    for (name, sql_type) in [
        ("company", "TEXT DEFAULT ''"),
        ("location", "TEXT DEFAULT ''"),
        ("salary", "TEXT DEFAULT ''"),
        ("source", "TEXT DEFAULT ''"),
        ("employment_type", "TEXT DEFAULT ''"),
        ("date_posted", "TEXT DEFAULT ''"),
    ] {
        if !columns.contains(name) {
            connection.execute(
                &format!("ALTER TABLE jobs ADD COLUMN {name} {sql_type}"),
                [],
            )?;
        }
    }
    if !table_columns(connection, "feed_state")?.contains("consecutive_failures") {
        connection.execute(
            "ALTER TABLE feed_state ADD COLUMN consecutive_failures INTEGER DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get(1))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn trim_table(tx: &Transaction<'_>, table: &str, max_items: usize) -> Result<()> {
    let count: i64 = tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })?;
    let excess = count.saturating_sub(max_items as i64);
    if excess > 0 {
        tx.execute(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} ORDER BY position LIMIT ?)",), [excess])?;
    }
    Ok(())
}

fn metadata(connection: &Connection, scope: &str) -> Result<HashMap<String, String>> {
    let mut statement = connection.prepare("SELECT key,value FROM state_metadata WHERE scope=?")?;
    let rows = statement.query_map([scope], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
fn get<'a>(map: &'a HashMap<String, String>, key: &str) -> &'a str {
    map.get(key).map(String::as_str).unwrap_or_default()
}

fn save_metadata(tx: &Transaction<'_>, scope: &str, state: &Value, keys: &[&str]) -> Result<()> {
    for key in keys {
        let value = state.get(*key).and_then(Value::as_str).unwrap_or_default();
        tx.execute("INSERT INTO state_metadata(scope,key,value) VALUES(?,?,?) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value", params![scope,key,value])?;
    }
    Ok(())
}

fn strings_from_table(connection: &Connection, query: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(query)?;
    let rows = statement.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
fn json_from_table(connection: &Connection, query: &str) -> Result<Vec<Value>> {
    Ok(strings_from_table(connection, query)?
        .into_iter()
        .filter_map(|value| serde_json::from_str(&value).ok())
        .collect())
}

fn save_application(tx: &Transaction<'_>, record: &Value, position: i64) -> Result<()> {
    let link = record
        .get("link")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if link.is_empty() {
        return Ok(());
    }
    tx.execute(
        "INSERT OR REPLACE INTO applications(position,link,payload_json) VALUES(?,?,?)",
        params![position, link, serde_json::to_string(record)?],
    )?;
    for value in record
        .get("links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .chain(std::iter::once(link))
    {
        tx.execute(
            "INSERT OR REPLACE INTO application_links(link_value,application_link) VALUES(?,?)",
            params![value, link],
        )?;
    }
    for value in record
        .get("review_fingerprints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        tx.execute("INSERT OR REPLACE INTO application_fingerprints(fingerprint,application_link) VALUES(?,?)", params![value,link])?;
    }
    Ok(())
}

pub fn write_json(path: impl AsRef<Path>, value: &Value) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub fn export_csv(path: impl AsRef<Path>, rows: &[JobRow]) -> Result<()> {
    let mut output =
        "time,title,company,location,salary,source,employment_type,date_posted,description,link\n"
            .to_owned();
    for row in rows {
        let fields = [
            &row.time,
            &row.lead.title,
            &row.lead.company,
            &row.lead.location,
            &row.lead.salary,
            &row.lead.source,
            &row.lead.employment_type,
            &row.lead.date_posted,
            &row.lead.description,
            &row.lead.link,
        ];
        output.push_str(
            &fields
                .into_iter()
                .map(|field| format!("\"{}\"", field.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(","),
        );
        output.push('\n');
    }
    fs::write(path, output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("nested").join("roleforge.sqlite3")).unwrap();
        (dir, db)
    }

    fn row(time: &str, title: &str, link: &str) -> JobRow {
        JobRow {
            time: time.into(),
            lead: JobLead {
                title: title.into(),
                link: link.into(),
                description: "Description".into(),
                company: "Acme".into(),
                location: "London".into(),
                salary: "£45000".into(),
                source: "feed".into(),
                employment_type: "Permanent".into(),
                date_posted: "2026-01-01".into(),
            },
        }
    }

    #[test]
    fn database_round_trip() {
        let (_dir, mut db) = db();
        db.append_jobs(&[row("2026-01-01T00:00:00Z", "Engineer", "https://example/1")])
            .unwrap();
        let jobs = db.jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].time, "2026-01-01T00:00:00Z");
        assert_eq!(jobs[0].lead, row("t", "Engineer", "https://example/1").lead);
    }

    #[test]
    fn append_jobs_ignores_repeated_links() {
        let (_dir, mut db) = db();
        db.append_jobs(&[
            row("t1", "Engineer", "https://example/1"),
            row("t1", "Analyst", "https://example/2"),
        ])
        .unwrap();
        db.append_jobs(&[row("t2", "Engineer again", "https://example/1")])
            .unwrap();
        let titles: Vec<_> = db
            .jobs()
            .unwrap()
            .into_iter()
            .map(|job| job.lead.title)
            .collect();
        assert_eq!(titles, ["Engineer", "Analyst"]);
    }

    #[test]
    fn latest_batch_returns_only_the_newest_timestamp() {
        let (_dir, mut db) = db();
        assert_eq!(db.latest_batch().unwrap(), (None, vec![]));

        db.append_jobs(&[
            row("2026-01-01T00:00:00Z", "Old", "https://example/1"),
            row("2026-01-02T00:00:00Z", "New", "https://example/2"),
            row("2026-01-02T00:00:00Z", "Newer", "https://example/3"),
        ])
        .unwrap();
        let (timestamp, items) = db.latest_batch().unwrap();
        assert_eq!(timestamp.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], "New");
        assert_eq!(items[1]["title"], "Newer");
        assert_eq!(items[0]["link"], "https://example/2");
        assert_eq!(items[0]["description"], "Description");
        assert_eq!(items[0]["time"], "2026-01-02T00:00:00Z");
    }

    #[test]
    fn feed_state_round_trips_and_updates_in_place() {
        let (_dir, mut db) = db();
        assert!(db.feed_state().unwrap().is_empty());

        let mut state = HashMap::from([(
            "wwr".to_owned(),
            FeedState {
                last_checked_at: 1234.5,
                consecutive_failures: 2,
            },
        )]);
        db.save_feed_state(&state).unwrap();
        let loaded = db.feed_state().unwrap();
        assert_eq!(loaded["wwr"].last_checked_at, 1234.5);
        assert_eq!(loaded["wwr"].consecutive_failures, 2);

        state.get_mut("wwr").unwrap().consecutive_failures = 0;
        db.save_feed_state(&state).unwrap();
        assert_eq!(db.feed_state().unwrap()["wwr"].consecutive_failures, 0);
        assert_eq!(db.feed_state().unwrap().len(), 1);
    }

    #[test]
    fn reviewed_fingerprints_are_recorded_and_trimmed_oldest_first() {
        let (_dir, mut db) = db();
        assert!(!db.has_reviewed(&["a".into()]).unwrap());
        assert!(!db.has_reviewed(&[]).unwrap());

        db.append_reviewed(&["a".into(), "b".into()], 10).unwrap();
        assert!(db.has_reviewed(&["a".into()]).unwrap());
        assert!(db.has_reviewed(&["zzz".into(), "b".into()]).unwrap());
        assert!(!db.has_reviewed(&["zzz".into()]).unwrap());

        // Re-inserting an existing fingerprint must not advance the position,
        // or the trim below would drop the wrong rows.
        db.append_reviewed(&["a".into(), "c".into()], 2).unwrap();
        assert!(!db.has_reviewed(&["a".into()]).unwrap());
        assert!(db.has_reviewed(&["b".into()]).unwrap());
        assert!(db.has_reviewed(&["c".into()]).unwrap());
    }

    #[test]
    fn alert_state_round_trips_links_pending_and_metadata() {
        let (_dir, mut db) = db();
        let empty = db.alert_state().unwrap();
        assert_eq!(empty["alerted_links"], json!([]));
        assert_eq!(empty["pending_alerts"], json!([]));
        assert_eq!(empty["last_run_utc"], "");

        db.save_alert_state(&json!({
            "alerted_links": ["https://example/1", "https://example/2"],
            "pending_alerts": [
                {"link": "https://example/3", "score": 40},
                {"score": 10}
            ],
            "last_run_utc": "2026-01-01T00:00:00Z",
            "last_delivery_utc": "2026-01-01T00:05:00Z",
            "last_delivery_error": "rate limited"
        }))
        .unwrap();

        let state = db.alert_state().unwrap();
        assert_eq!(
            state["alerted_links"],
            json!(["https://example/1", "https://example/2"])
        );
        // A pending alert with no link is dropped.
        assert_eq!(state["pending_alerts"].as_array().unwrap().len(), 1);
        assert_eq!(state["pending_alerts"][0]["score"], 40);
        assert_eq!(state["last_run_utc"], "2026-01-01T00:00:00Z");
        assert_eq!(state["last_delivery_utc"], "2026-01-01T00:05:00Z");
        assert_eq!(state["last_delivery_error"], "rate limited");

        // Saving again replaces rather than appends, and clears absent metadata.
        db.save_alert_state(&json!({"alerted_links": ["https://example/9"]}))
            .unwrap();
        let state = db.alert_state().unwrap();
        assert_eq!(state["alerted_links"], json!(["https://example/9"]));
        assert_eq!(state["pending_alerts"], json!([]));
        assert_eq!(state["last_delivery_error"], "");
    }

    #[test]
    fn applications_state_round_trips_records_and_metadata() {
        let (_dir, mut db) = db();
        let empty = db.applications_state().unwrap();
        assert_eq!(empty["applications"], json!([]));
        assert_eq!(empty["last_updated_utc"], "");
        assert_eq!(empty["last_cleanup_utc"], "");

        db.save_applications_state(&json!({
            "applications": [
                {"link": "https://example/1", "status": "new",
                 "links": ["https://mirror/1"], "review_fingerprints": ["fp1"]},
                {"status": "no link here"}
            ],
            "last_updated_utc": "u",
            "last_digest_utc": "d",
            "last_digest_date_utc": "dd",
            "last_digest_error": "de",
            "last_feedback_utc": "f",
            "last_cleanup_utc": "c"
        }))
        .unwrap();

        let state = db.applications_state().unwrap();
        assert_eq!(state["applications"].as_array().unwrap().len(), 1);
        assert_eq!(state["applications"][0]["status"], "new");
        assert_eq!(state["last_updated_utc"], "u");
        assert_eq!(state["last_digest_utc"], "d");
        assert_eq!(state["last_digest_date_utc"], "dd");
        assert_eq!(state["last_digest_error"], "de");
        assert_eq!(state["last_feedback_utc"], "f");
        assert_eq!(state["last_cleanup_utc"], "c");
    }

    #[test]
    fn upsert_application_reports_only_the_first_sighting() {
        let (_dir, mut db) = db();
        let record = json!({"link": "https://example/1", "status": "new"});
        assert!(db.upsert_application(&record).unwrap());
        assert!(!db.upsert_application(&record).unwrap());
        assert!(
            !db.upsert_application(&json!({"status": "no link"}))
                .unwrap()
        );

        let stored = db.applications_state().unwrap();
        assert_eq!(stored["applications"].as_array().unwrap().len(), 1);

        assert!(
            db.upsert_application(&json!({"link": "https://example/2"}))
                .unwrap()
        );
        assert_eq!(
            db.applications_state().unwrap()["applications"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn telegram_offset_defaults_to_zero_and_survives_updates() {
        let (_dir, db) = db();
        assert_eq!(db.telegram_offset().unwrap(), 0);
        db.save_telegram_offset(42).unwrap();
        assert_eq!(db.telegram_offset().unwrap(), 42);
        db.save_telegram_offset(-7).unwrap();
        assert_eq!(db.telegram_offset().unwrap(), -7);
    }

    #[test]
    fn digest_sessions_round_trip_and_keep_only_the_newest_twenty() {
        let (_dir, db) = db();
        assert_eq!(db.digest_session("missing").unwrap(), None);

        db.save_digest_session("abc", "2026-01-01T00:00:00Z", &["page one".into()])
            .unwrap();
        assert_eq!(
            db.digest_session("abc").unwrap(),
            Some(vec!["page one".to_owned()])
        );

        // Replacing an id keeps one row, not two.
        db.save_digest_session("abc", "2026-01-01T00:00:01Z", &["replaced".into()])
            .unwrap();
        assert_eq!(
            db.digest_session("abc").unwrap(),
            Some(vec!["replaced".to_owned()])
        );

        for index in 0..21 {
            db.save_digest_session(
                &format!("s{index}"),
                &format!("2026-02-{:02}T00:00:00Z", index + 1),
                &[format!("page {index}")],
            )
            .unwrap();
        }
        assert_eq!(db.digest_session("s0").unwrap(), None);
        assert_eq!(
            db.digest_session("s20").unwrap(),
            Some(vec!["page 20".to_owned()])
        );
    }

    #[test]
    fn open_migrates_a_database_missing_later_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE jobs(id INTEGER PRIMARY KEY AUTOINCREMENT,time TEXT NOT NULL,
                    title TEXT NOT NULL,description TEXT NOT NULL,link TEXT NOT NULL UNIQUE);
                 CREATE TABLE feed_state(name TEXT PRIMARY KEY,last_checked_at REAL NOT NULL);
                 INSERT INTO jobs(time,title,description,link)
                    VALUES('t','Legacy','Desc','https://example/legacy');",
            )
            .unwrap();
        drop(connection);

        let mut db = Database::open(&path).unwrap();
        let jobs = db.jobs().unwrap();
        assert_eq!(jobs[0].lead.title, "Legacy");
        assert_eq!(jobs[0].lead.company, "");
        db.save_feed_state(&HashMap::from([("wwr".to_owned(), FeedState::default())]))
            .unwrap();
        assert_eq!(db.feed_state().unwrap()["wwr"].consecutive_failures, 0);
    }

    #[test]
    fn writes_json_and_csv_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("out.json");
        write_json(&json_path, &json!({"count": 1})).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&fs::read_to_string(&json_path).unwrap()).unwrap(),
            json!({"count": 1})
        );

        let csv_path = dir.path().join("out.csv");
        let mut quoted = row("t", "Engineer", "https://example/1");
        quoted.lead.company = "Acme \"Ltd\"".into();
        export_csv(&csv_path, &[quoted]).unwrap();
        let csv = fs::read_to_string(&csv_path).unwrap();
        let mut lines = csv.lines();
        assert_eq!(
            lines.next().unwrap(),
            "time,title,company,location,salary,source,employment_type,date_posted,description,link"
        );
        assert_eq!(
            lines.next().unwrap(),
            "\"t\",\"Engineer\",\"Acme \"\"Ltd\"\"\",\"London\",\"£45000\",\"feed\",\"Permanent\",\"2026-01-01\",\"Description\",\"https://example/1\""
        );
        assert_eq!(lines.next(), None);

        export_csv(&csv_path, &[]).unwrap();
        assert_eq!(fs::read_to_string(&csv_path).unwrap().lines().count(), 1);
    }
}
