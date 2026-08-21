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
    #[test]
    fn database_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "roleforge-{}-{}.sqlite3",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let mut db = Database::open(&path).unwrap();
        db.append_jobs(&[JobRow {
            time: "2026-01-01T00:00:00Z".into(),
            lead: JobLead {
                title: "Engineer".into(),
                link: "https://example/1".into(),
                ..JobLead::default()
            },
        }])
        .unwrap();
        assert_eq!(db.jobs().unwrap().len(), 1);
        assert_eq!(db.latest_batch().unwrap().1[0]["title"], "Engineer");
        drop(db);
        fs::remove_file(path).unwrap();
    }
}
