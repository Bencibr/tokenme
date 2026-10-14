//! Why a probe that ran answered nothing — one line, for the host's own log.
//!
//! The panel's quota log has always said which probes were refused (its own
//! switch, an absent host) and what the rest answered. The case it could not
//! name was the emptiest one: a probe that was allowed, ran, and came back with
//! nothing. `http::get_json` drops non-2xx bodies, so a vendor's `401`/`403` —
//! and `api_key()` finding no credential at all — were indistinguishable from
//! "this tool has no quota surface", and the log line simply listed fewer
//! windows than tools.
//!
//! That blind spot is how a live OpenCode Go subscription stayed invisible for
//! days: V2 moved the credential out of `auth.json` into its own database, the
//! probe found only a stale relay key, the vendor answered `401`, and every
//! surface said "no row" with no reason attached. A probe now records one short
//! line per tool; the host drains them once per pass and prints them next to it.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

fn table() -> &'static Mutex<BTreeMap<&'static str, String>> {
    static NOTES: OnceLock<Mutex<BTreeMap<&'static str, String>>> = OnceLock::new();
    NOTES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Record why `tool`'s probe came back empty. A later call for the same tool
/// replaces the line: one pass has one answer per tool, not a transcript.
pub(crate) fn note(tool: &'static str, why: impl Into<String>) {
    // A panicking probe leaves the mutex poisoned, not the line wrong; losing
    // the note is worse than recovering the map.
    let mut guard = table().lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(tool, why.into());
}

/// Drain what this pass's probes had to say, tool-sorted. The host calls it
/// once after a pass; a probe served from cache wrote nothing and stays silent,
/// which is exactly the difference the log needs to show.
pub fn take() -> Vec<(&'static str, String)> {
    let mut guard = table().lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *guard).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_is_read_once_and_replaced_within_a_pass() {
        note("notes-test-tool", "HTTP 401");
        note("notes-test-tool", "HTTP 403 EntitlementError");
        let taken = take();
        assert_eq!(taken, vec![("notes-test-tool", "HTTP 403 EntitlementError".to_string())]);
        assert!(take().is_empty(), "a drained pass says nothing twice");
    }

    #[test]
    fn notes_come_back_in_tool_order() {
        note("notes-test-b", "no official credential");
        note("notes-test-a", "HTTP 401");
        let tools: Vec<&str> = take().into_iter().map(|(tool, _)| tool).collect();
        assert_eq!(tools, vec!["notes-test-a", "notes-test-b"]);
    }
}
