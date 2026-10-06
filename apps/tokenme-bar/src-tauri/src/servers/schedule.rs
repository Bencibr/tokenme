//! Scheduling math for server pulls: when the next attempt is due, how fast
//! retries back off, and what health bucket the badge dot shows. Pure
//! functions — the hub owns the clocks.

use std::collections::BTreeMap;
use std::time::Instant;

use super::ServerRecord;

/// Retry backoff after `fail_count` consecutive failures: 1 → 2 → 4 → 8 → 16
/// → 30 minutes (capped). A success resets the count, so the next failure
/// starts at one minute again.
pub fn backoff_secs(fail_count: u32) -> u64 {
    let n = fail_count.clamp(1, 20);
    (60u64 << (n - 1)).min(1800)
}

/// How stale `last_ok` may get before the dot turns amber: two missed
/// cadences plus five minutes of grace (a pull that is 2 × every + 5 min old
/// means at least one attempt failed to land or the machine was asleep).
pub fn warn_after_ms(every_secs: u64) -> i64 {
    2 * (every_secs as i64) * 1000 + 300_000
}

/// The badge dot / detail status. Priority: an in-flight pull beats a stale
/// error; an error beats amber; "registered but never synced" is `none`.
pub fn derive_status(rec: &ServerRecord, pulling: bool, now_ms: i64) -> &'static str {
    if pulling {
        return "syncing";
    }
    if rec.last_error.is_some() {
        return "error";
    }
    match rec.last_ok_ms {
        None => "none",
        Some(last) if now_ms.saturating_sub(last) > warn_after_ms(rec.every_secs) => "warn",
        Some(_) => "ok",
    }
}

/// The ids the hub should pull now: enabled, not already in flight, and due
/// (an id with no scheduled time is due immediately — e.g. just re-enabled).
pub fn due_ids(
    servers: &[ServerRecord],
    next_due: &BTreeMap<u64, Instant>,
    pulling: &std::collections::BTreeSet<u64>,
    now: Instant,
) -> Vec<u64> {
    servers
        .iter()
        .filter(|r| r.enabled && !pulling.contains(&r.id))
        .filter(|r| next_due.get(&r.id).map_or(true, |t| *t <= now))
        .map(|r| r.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(3), 240);
        assert_eq!(backoff_secs(4), 480);
        assert_eq!(backoff_secs(5), 960);
        assert_eq!(backoff_secs(6), 1800); // 1920 → cap
        assert_eq!(backoff_secs(50), 1800);
        assert_eq!(backoff_secs(0), 60); // clamped up, never zero
    }

    #[test]
    fn status_priority() {
        let mut rec = rec_with(None, None, 900);
        assert_eq!(derive_status(&rec, false, 1_000_000), "none");
        assert_eq!(derive_status(&rec, true, 1_000_000), "syncing");

        rec.last_ok_ms = Some(1_000_000);
        assert_eq!(derive_status(&rec, false, 1_000_000 + 1000), "ok");
        // 2 × 900 s + 300 s grace: one ms past the line is amber.
        let line = warn_after_ms(900);
        assert_eq!(derive_status(&rec, false, 1_000_000 + line), "ok");
        assert_eq!(derive_status(&rec, false, 1_000_000 + line + 1), "warn");

        rec.last_error = Some(super::super::SshError::new("timeout", "no route"));
        assert_eq!(derive_status(&rec, false, 1_000_000 + line + 1), "error");
        assert_eq!(derive_status(&rec, true, 1_000_000 + line + 1), "syncing");
    }

    #[test]
    fn due_selection() {
        let now = Instant::now();
        let mut a = rec_with(Some(1_000), None, 300);
        a.id = 1;
        a.enabled = true;
        let mut b = rec_with(Some(1_000), None, 300);
        b.id = 2;
        b.enabled = false;
        let mut c = rec_with(None, None, 300);
        c.id = 3;
        c.enabled = true;
        let servers = vec![a, b, c];
        let mut due = BTreeMap::new();
        due.insert(1u64, now + std::time::Duration::from_secs(60));
        let pulling = std::collections::BTreeSet::new();
        // 1 not yet due, 2 disabled, 3 has no scheduled time → due now.
        assert_eq!(due_ids(&servers, &due, &pulling, now), vec![3]);
        // 1 becomes due when its time passes.
        assert_eq!(due_ids(&servers, &due, &pulling, now + std::time::Duration::from_secs(61)), vec![1, 3]);
        // A server already pulling is skipped even when due.
        let mut pulling = std::collections::BTreeSet::new();
        pulling.insert(3u64);
        assert_eq!(due_ids(&servers, &due, &pulling, now + std::time::Duration::from_secs(61)), vec![1]);
    }

    fn rec_with(last_ok: Option<i64>, last_err: Option<super::super::SshError>, every: u64) -> ServerRecord {
        ServerRecord {
            id: 1,
            name: "n".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            fingerprint: "f".into(),
            every_secs: every,
            days: 30,
            enabled: true,
            created_at_ms: 0,
            last_ok_ms: last_ok,
            last_error: last_err,
            fail_count: 0,
            last_rows: 0,
            last_took_ms: 0,
            tools: vec![],
            history: vec![],
        }
    }
}
