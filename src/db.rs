//! One SQLite file in WAL mode. Raw checks are kept 7 days, hourly rollups 90 days.

use crate::util::now;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub const RAW_DAYS: i64 = 7;
pub const ROLLUP_DAYS: i64 = 90;
pub const BUCKET: i64 = 900; // 15 minutes
pub const BUCKETS: usize = 96; // 24 h

pub struct Db {
    conn: Mutex<Connection>,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CheckRow {
    pub target: String,
    pub ts: i64,
    pub ok: bool,
    pub ms: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Bucket {
    pub n: u32,
    pub ok: u32,
    pub lat: Option<f64>,
}

impl Bucket {
    pub fn uptime(&self) -> Option<f64> {
        (self.n > 0).then(|| f64::from(self.ok) / f64::from(self.n))
    }
}

#[derive(Clone, Debug)]
pub struct Outage {
    pub id: i64,
    pub target: String,
    pub started: i64,
    pub ended: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct AlertRow {
    pub id: i64,
    pub created: i64,
    pub title: String,
    pub body: String,
    pub priority: i64,
    pub attempts: i64,
}

#[derive(Clone, Debug)]
pub struct CamEvent {
    pub source: String,
    pub device: String,
    pub kind: String,
    pub ts: i64,
}

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS checks(target TEXT NOT NULL, ts INTEGER NOT NULL, ok INTEGER NOT NULL, latency_ms REAL);
CREATE INDEX IF NOT EXISTS checks_ts ON checks(ts);
CREATE INDEX IF NOT EXISTS checks_target_ts ON checks(target, ts);
CREATE TABLE IF NOT EXISTS rollups(target TEXT NOT NULL, hour INTEGER NOT NULL, n INTEGER NOT NULL, ok INTEGER NOT NULL,
  lat_avg REAL, lat_max REAL, PRIMARY KEY(target, hour)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS outages(id INTEGER PRIMARY KEY, target TEXT NOT NULL, started INTEGER NOT NULL, ended INTEGER);
CREATE INDEX IF NOT EXISTS outages_target ON outages(target, started);
CREATE TABLE IF NOT EXISTS alerts(id INTEGER PRIMARY KEY, created INTEGER NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
  priority INTEGER NOT NULL, outage_key TEXT, kind TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
  next_try INTEGER NOT NULL, sent INTEGER);
CREATE INDEX IF NOT EXISTS alerts_pending ON alerts(sent, next_try);
CREATE TABLE IF NOT EXISTS sessions(token_hash TEXT PRIMARY KEY, csrf TEXT NOT NULL, created INTEGER NOT NULL, last_seen INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS cam_events(source TEXT NOT NULL, device TEXT NOT NULL, kind TEXT NOT NULL, ts INTEGER NOT NULL,
  PRIMARY KEY(source, device, kind, ts)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS cam_events_ts ON cam_events(ts);
CREATE TABLE IF NOT EXISTS integration_state(name TEXT PRIMARY KEY, data TEXT NOT NULL);
";

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        // auto_vacuum only takes effect on a brand-new file, before any table exists.
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;
             PRAGMA cache_size = -1024; PRAGMA temp_store = MEMORY; PRAGMA mmap_size = 0;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn: Mutex::new(conn), path: path.to_path_buf() })
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- meta ------------------------------------------------------------

    pub fn meta(&self, key: &str) -> Option<String> {
        self.c().query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        let _ = self.c().execute("INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [key, value]);
    }

    // ---- checks ----------------------------------------------------------

    pub fn insert_checks(&self, rows: &[CheckRow]) {
        if rows.is_empty() {
            return;
        }
        let mut c = self.c();
        let Ok(tx) = c.transaction() else { return };
        {
            let Ok(mut st) = tx.prepare_cached("INSERT INTO checks(target, ts, ok, latency_ms) VALUES(?1, ?2, ?3, ?4)") else { return };
            for r in rows {
                let _ = st.execute(params![r.target, r.ts, r.ok, r.ms]);
            }
        }
        let _ = tx.commit();
    }

    /// 96 fifteen-minute buckets per target, oldest first, ending with the current bucket.
    pub fn buckets_24h(&self) -> (i64, HashMap<String, [Bucket; BUCKETS]>) {
        let start = (now() / BUCKET) * BUCKET - (BUCKETS as i64 - 1) * BUCKET;
        let mut out: HashMap<String, [Bucket; BUCKETS]> = HashMap::new();
        let c = self.c();
        let Ok(mut st) = c.prepare_cached(
            "SELECT target, (ts - ?1) / 900, COUNT(*), SUM(ok), AVG(CASE WHEN ok = 1 THEN latency_ms END)
             FROM checks WHERE ts >= ?1 GROUP BY target, 2",
        ) else {
            return (start, out);
        };
        let rows = st.query_map([start], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, u32>(2)?, r.get::<_, u32>(3)?, r.get::<_, Option<f64>>(4)?)));
        if let Ok(rows) = rows {
            for (t, b, n, ok, lat) in rows.flatten() {
                if (0..BUCKETS as i64).contains(&b) {
                    out.entry(t).or_insert([Bucket::default(); BUCKETS])[b as usize] = Bucket { n, ok, lat };
                }
            }
        }
        (start, out)
    }

    /// (day start, checks, ok) for the last `days` local-ish days (UTC-aligned buckets of 24 h from now).
    pub fn daily_uptime(&self, target: &str, days: i64) -> Vec<(i64, u32, u32)> {
        let end = now();
        let start = end - days * 86_400;
        let c = self.c();
        let mut out = vec![];
        if let Ok(mut st) = c.prepare_cached("SELECT (ts - ?2) / 86400, COUNT(*), SUM(ok) FROM checks WHERE target = ?1 AND ts >= ?2 GROUP BY 1 ORDER BY 1")
            && let Ok(rows) = st.query_map(params![target, start], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?, r.get::<_, u32>(2)?))) {
                for (d, n, ok) in rows.flatten() {
                    out.push((start + d * 86_400, n, ok));
                }
            }
        out
    }

    /// Uptime over the last 90 days from rollups plus raw checks not rolled up yet.
    pub fn uptime_90d(&self, target: &str) -> Option<f64> {
        let c = self.c();
        let since_hour = (now() - ROLLUP_DAYS * 86_400) / 3600;
        let rolled: (Option<i64>, Option<i64>, Option<i64>) = c
            .query_row("SELECT SUM(n), SUM(ok), MAX(hour) FROM rollups WHERE target = ?1 AND hour >= ?2", params![target, since_hour], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .ok()?;
        let raw_from = rolled.2.map(|h| (h + 1) * 3600).unwrap_or(0);
        let raw: (Option<i64>, Option<i64>) =
            c.query_row("SELECT COUNT(*), SUM(ok) FROM checks WHERE target = ?1 AND ts >= ?2", params![target, raw_from], |r| Ok((r.get(0)?, r.get(1)?))).ok()?;
        let n = rolled.0.unwrap_or(0) + raw.0.unwrap_or(0);
        let ok = rolled.1.unwrap_or(0) + raw.1.unwrap_or(0);
        (n > 0).then(|| ok as f64 / n as f64)
    }

    // ---- outages ---------------------------------------------------------

    pub fn open_outage(&self, target: &str, started: i64) -> i64 {
        let c = self.c();
        let _ = c.execute("INSERT INTO outages(target, started) VALUES(?1, ?2)", params![target, started]);
        c.last_insert_rowid()
    }

    pub fn close_outage(&self, id: i64, ended: i64) {
        let _ = self.c().execute("UPDATE outages SET ended = ?2 WHERE id = ?1", params![id, ended]);
    }

    pub fn open_outages(&self) -> Vec<Outage> {
        self.outages_where("ended IS NULL", None, 1000)
    }

    pub fn outages(&self, target: Option<&str>, limit: usize) -> Vec<Outage> {
        match target {
            Some(t) => self.outages_where("target = ?1", Some(t), limit),
            None => self.outages_where("1", None, limit),
        }
    }

    fn outages_where(&self, cond: &str, arg: Option<&str>, limit: usize) -> Vec<Outage> {
        let c = self.c();
        let sql = format!("SELECT id, target, started, ended FROM outages WHERE {cond} ORDER BY started DESC LIMIT {limit}");
        let Ok(mut st) = c.prepare(&sql) else { return vec![] };
        let map = |r: &rusqlite::Row| Ok(Outage { id: r.get(0)?, target: r.get(1)?, started: r.get(2)?, ended: r.get(3)? });
        let rows = match arg {
            Some(a) => st.query_map([a], map).map(|it| it.flatten().collect()),
            None => st.query_map([], map).map(|it| it.flatten().collect()),
        };
        rows.unwrap_or_default()
    }

    // ---- alerts ----------------------------------------------------------

    pub fn insert_alert(&self, title: &str, body: &str, priority: i64, kind: &str, outage_key: Option<&str>) -> i64 {
        let c = self.c();
        let t = now();
        let _ = c.execute(
            "INSERT INTO alerts(created, title, body, priority, kind, outage_key, next_try) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?1)",
            params![t, title, body, priority, kind, outage_key],
        );
        c.last_insert_rowid()
    }

    /// If the matching "down" alert never went out, drop it and return its creation time.
    pub fn take_unsent_down(&self, outage_key: &str) -> Option<i64> {
        let c = self.c();
        let row: Option<(i64, i64)> = c
            .query_row("SELECT id, created FROM alerts WHERE outage_key = ?1 AND kind = 'down' AND sent IS NULL ORDER BY id DESC LIMIT 1", [outage_key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .ok()
            .flatten();
        let (id, created) = row?;
        let _ = c.execute("DELETE FROM alerts WHERE id = ?1", [id]);
        Some(created)
    }

    pub fn due_alerts(&self, limit: usize) -> Vec<AlertRow> {
        let c = self.c();
        let Ok(mut st) = c.prepare_cached("SELECT id, created, title, body, priority, attempts FROM alerts WHERE sent IS NULL AND next_try <= ?1 ORDER BY id LIMIT ?2") else {
            return vec![];
        };
        st.query_map(params![now(), limit as i64], |r| Ok(AlertRow { id: r.get(0)?, created: r.get(1)?, title: r.get(2)?, body: r.get(3)?, priority: r.get(4)?, attempts: r.get(5)? }))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    }

    pub fn pending_alerts(&self) -> i64 {
        self.c().query_row("SELECT COUNT(*) FROM alerts WHERE sent IS NULL", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn mark_alert(&self, id: i64, sent: Option<i64>, next_try: i64) {
        let _ = self.c().execute("UPDATE alerts SET sent = ?2, attempts = attempts + 1, next_try = ?3 WHERE id = ?1", params![id, sent, next_try]);
    }

    // ---- sessions ----------------------------------------------------------

    pub fn create_session(&self, hash: &str, csrf: &str) {
        let t = now();
        let _ = self.c().execute("INSERT INTO sessions(token_hash, csrf, created, last_seen) VALUES(?1, ?2, ?3, ?3)", params![hash, csrf, t]);
    }

    pub fn session(&self, hash: &str) -> Option<(String, i64)> {
        self.c().query_row("SELECT csrf, last_seen FROM sessions WHERE token_hash = ?1", [hash], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten()
    }

    pub fn touch_session(&self, hash: &str) {
        let _ = self.c().execute("UPDATE sessions SET last_seen = ?2 WHERE token_hash = ?1", params![hash, now()]);
    }

    pub fn delete_session(&self, hash: &str) {
        let _ = self.c().execute("DELETE FROM sessions WHERE token_hash = ?1", [hash]);
    }

    pub fn delete_all_sessions(&self) {
        let _ = self.c().execute("DELETE FROM sessions", []);
    }

    // ---- camera events / integration state ---------------------------------

    /// Returns true when the event is new.
    pub fn insert_cam_event(&self, e: &CamEvent) -> bool {
        self.c()
            .execute("INSERT OR IGNORE INTO cam_events(source, device, kind, ts) VALUES(?1, ?2, ?3, ?4)", params![e.source, e.device, e.kind, e.ts])
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    pub fn cam_events_since(&self, since: i64) -> Vec<CamEvent> {
        let c = self.c();
        let Ok(mut st) = c.prepare_cached("SELECT source, device, kind, ts FROM cam_events WHERE ts >= ?1 ORDER BY ts DESC LIMIT 400") else { return vec![] };
        st.query_map([since], |r| Ok(CamEvent { source: r.get(0)?, device: r.get(1)?, kind: r.get(2)?, ts: r.get(3)? })).map(|it| it.flatten().collect()).unwrap_or_default()
    }

    pub fn save_integration(&self, name: &str, data: &str) {
        let _ = self.c().execute("INSERT INTO integration_state(name, data) VALUES(?1, ?2) ON CONFLICT(name) DO UPDATE SET data = excluded.data", [name, data]);
    }

    pub fn load_integration(&self, name: &str) -> Option<String> {
        self.c().query_row("SELECT data FROM integration_state WHERE name = ?1", [name], |r| r.get(0)).optional().ok().flatten()
    }

    // ---- retention -----------------------------------------------------------

    /// Roll finished hours into `rollups`, then prune old rows. Runs hourly.
    pub fn maintain(&self) {
        let t = now();
        let current_hour = t / 3600;
        let last: i64 = self.meta("rolled_until_hour").and_then(|v| v.parse().ok()).unwrap_or(current_hour - RAW_DAYS * 24);
        let c = self.c();
        if last < current_hour - 1 {
            let from = (last + 1) * 3600;
            let to = current_hour * 3600;
            let _ = c.execute(
                "INSERT OR REPLACE INTO rollups(target, hour, n, ok, lat_avg, lat_max)
                 SELECT target, ts / 3600, COUNT(*), SUM(ok), AVG(CASE WHEN ok = 1 THEN latency_ms END), MAX(latency_ms)
                 FROM checks WHERE ts >= ?1 AND ts < ?2 GROUP BY target, ts / 3600",
                params![from, to],
            );
            let _ = c.execute(
                "INSERT INTO meta(key, value) VALUES('rolled_until_hour', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [(current_hour - 1).to_string()],
            );
        }
        let _ = c.execute("DELETE FROM checks WHERE ts < ?1", [t - RAW_DAYS * 86_400]);
        let _ = c.execute("DELETE FROM rollups WHERE hour < ?1", [(t - ROLLUP_DAYS * 86_400) / 3600]);
        let _ = c.execute("DELETE FROM outages WHERE ended IS NOT NULL AND ended < ?1", [t - ROLLUP_DAYS * 86_400]);
        let _ = c.execute("DELETE FROM alerts WHERE created < ?1", [t - 30 * 86_400]);
        let _ = c.execute("DELETE FROM cam_events WHERE ts < ?1", [t - ROLLUP_DAYS * 86_400]);
        let _ = c.execute("DELETE FROM sessions WHERE last_seen < ?1", [t - 30 * 86_400]);
        let _ = c.execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);");
    }

    pub fn size_bytes(&self) -> u64 {
        let mut total = 0;
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            total += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        }
        total
    }

    pub fn row_counts(&self) -> Vec<(&'static str, i64)> {
        let c = self.c();
        ["checks", "rollups", "outages", "alerts", "cam_events"]
            .into_iter()
            .map(|t| (t, c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0)).unwrap_or(0)))
            .collect()
    }
}
