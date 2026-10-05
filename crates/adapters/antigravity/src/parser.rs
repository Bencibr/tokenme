//! The protobuf blobs Antigravity CLI writes, decoded into token stages.
//!
//! Field numbers were reverse-engineered against real databases and are
//! cross-checked by two independent open-source decoders — `junhoyeo/tokscale`
//! (Rust, hand-written wire reader) and `xiufengsun/TokenTracker` (JS) — plus
//! ccusage for the root list. Where those disagree the majority wins and the
//! disagreement is kept out of the totals; each such call has a comment on the
//! field it belongs to.
//!
//! The record layout, verified over 83 `gen_metadata` rows on this machine:
//!
//! - `#1`              → the chat-model message
//!   - `#3`  varint    → the same fixed prefix count as `#1.#4.1`
//!   - `#4`  message   → **usage**; see [`counts_for`]
//!   - `#9`  message   → per-generation timing: a Timestamp at `#4` on agy
//!     ≤ 1.1.17, `#2` = `u64::MAX` ("unset") plus opaque `#10` bytes on 1.1.18+
//!   - `#11`/`#12`     → latency histograms, unused
//!   - `#17` repeated  → the attempts one response burned through. Measured over
//!     81,727 items in 57 local databases: 81,260 are a byte-for-byte copy of
//!     `#4` under the *same* responseId, 435 carry no responseId and all-zero
//!     usage, 15 differ from `#4` under the parent's own id, and only **17 have
//!     an id of their own** — those are the separate calls, and a copy counted as
//!     one would double every turn. See [`decode_retries`].
//!   - `#19` string    → machine model id (`gemini-3.6-flash`) — the model
//!   - `#21` string    → server-supplied display label, never a pricing key
//! - `usage`: `#1` prefix, `#2` uncached input, `#3` total output, `#5` cache
//!   read, `#9` + `#10` the output split, `#11` responseId.

use usage_core::TokenCounts;

use crate::wire::{broken_at, bytes_field, bytes_fields, string_field, varint_field, Reader};

/// `gen_metadata.data` → the chat-model message.
pub(crate) const F_CHAT_MODEL: u32 = 1;
/// Inside the chat-model message.
pub(crate) const CM_TIMING: u32 = 9;
pub(crate) const CM_USAGE: u32 = 4;
pub(crate) const CM_MODEL_ID: u32 = 19;
/// The repeated attempt box: one item per failover attempt this response burned.
pub(crate) const CM_RETRIES: u32 = 17;
/// Inside a [`CM_RETRIES`] item, the attempt's own usage message.
pub(crate) const RI_USAGE: u32 = 2;
/// Inside `usage`.
pub(crate) const U_PREFIX: u32 = 1;
pub(crate) const U_INPUT: u32 = 2;
pub(crate) const U_OUTPUT_TOTAL: u32 = 3;
pub(crate) const U_CACHE_READ: u32 = 5;
pub(crate) const U_OUT_A: u32 = 9;
pub(crate) const U_OUT_B: u32 = 10;
pub(crate) const U_RESPONSE_ID: u32 = 11;
/// Inside `#9`, the per-generation timing message.
pub(crate) const T_UNSET: u32 = 2;
pub(crate) const T_TIMESTAMP: u32 = 4;
pub(crate) const T_OPAQUE: u32 = 10;
/// Inside `steps.metadata`: the turn's own Timestamp, the usage message whose
/// `#11` is the same responseId `gen_metadata` writes, and the gen-index box.
pub(crate) const S_TIMESTAMP: u32 = 1;
pub(crate) const S_USAGE: u32 = 9;
pub(crate) const S_GEN_REF: u32 = 20;
/// Inside [`S_GEN_REF`], the 0-indexed `gen_metadata.idx` this step generated.
pub(crate) const G_GEN_IDX: u32 = 3;
/// `trajectory_metadata_blob.data`: the conversation's created-at, its first
/// workspace folder, and that folder's `file://` URI.
pub(crate) const B_CREATED: u32 = 2;
pub(crate) const B_FOLDER: u32 = 1;
pub(crate) const B_FOLDER_URI: u32 = 1;

/// A single stage of one call cannot legitimately reach this. Larger means a
/// field number moved under a schema change, and one bogus row would otherwise
/// swamp every bucket in the report.
const IMPLAUSIBLE_TOKENS: u64 = 1_000_000_000_000;

/// Why a row did not become an event. The offset in the last two variants is the
/// byte position where the wire scan gave up, so a future schema change is
/// auditable rather than merely invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    /// No `#1` chat-model message: not a generation record at all.
    NoChatModel,
    /// `#1.#4` absent, empty or all zero — an aborted or preamble turn.
    NotBillable,
    /// The buffer stopped decoding mid-record.
    Truncated(usize),
    /// A stage decoded past [`IMPLAUSIBLE_TOKENS`], i.e. a mis-read field.
    OutOfRange(usize),
}

/// The usage stages exactly as the wire carries them, before any mapping choice.
///
/// Merging happens on these numbers rather than on [`TokenCounts`], so the two
/// derived stages (`output` from `#3` or from the split, `reasoning` from `#10`)
/// can never drift away from the values they came out of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stages {
    /// `#4.1` — the fixed system-and-tools prefix. Read, never billed.
    pub prefix: u64,
    /// `#4.2` — prompt tokens that did not come out of the cache.
    pub input: u64,
    /// `#4.3` — the server's own total output.
    pub total_output: u64,
    /// `#4.5` — cache read.
    pub cache_read: u64,
    /// `#4.9` — one half of the output split.
    pub out_a: u64,
    /// `#4.10` — the other half.
    pub out_b: u64,
}

impl Stages {
    /// Field-wise maximum. Streamed partials of one response each report a
    /// snapshot of the same call, so the largest value seen is the finished count
    /// and a sum would invent spend — the same rule Claude Code needs.
    pub(crate) fn max_each(self, other: Self) -> Self {
        Stages {
            prefix: self.prefix.max(other.prefix),
            input: self.input.max(other.input),
            total_output: self.total_output.max(other.total_output),
            cache_read: self.cache_read.max(other.cache_read),
            out_a: self.out_a.max(other.out_a),
            out_b: self.out_b.max(other.out_b),
        }
    }
}

/// One `gen_metadata` row, fully read: the turn the response finished as, plus
/// the earlier attempts of it that the server billed as separate calls.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Decoded {
    pub generation: Generation,
    pub retries: Vec<Generation>,
}

/// One decoded generation, before timestamp recovery and dedupe.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Generation {
    /// `gen_metadata.idx`, the table's integer primary key. Stable per database,
    /// so it doubles as the fallback dedupe key and as the join column for
    /// `steps.metadata.#20.#3`.
    pub idx: i64,
    pub model: Option<String>,
    pub counts: TokenCounts,
    pub stages: Stages,
    pub response_id: Option<String>,
    /// `#9.#4`, when this agy version still writes an explicit stamp.
    pub own_timestamp: Option<i64>,
    /// The 1.1.18 reading of `#9`, already gated on [`Lifetime`] and therefore
    /// only ever `Some` when it cannot be a coincidence.
    pub inferred_timestamp: Option<i64>,
}

/// Decodes one `gen_metadata.data` blob. `lifetime` bounds the timestamp guess;
/// `None` disables guessing rather than weakening it.
pub(crate) fn decode_generation(idx: i64, blob: &[u8], lifetime: Option<Lifetime>) -> Result<Decoded, Skip> {
    let chat_model = bytes_field(blob, F_CHAT_MODEL).ok_or(Skip::NoChatModel)?;
    // A torn envelope means the fields past the break were never reachable, so
    // nothing about the row can be trusted: say where it stopped and drop it.
    if let Some(offset) = broken_at(chat_model) {
        return Err(Skip::Truncated(offset));
    }
    let usage = bytes_field(chat_model, CM_USAGE).ok_or(Skip::NotBillable)?;
    if let Some(offset) = broken_at(usage) {
        return Err(Skip::Truncated(offset));
    }
    let stages = read_stages(usage).ok_or(Skip::OutOfRange(offset_of(usage, U_INPUT)))?;
    let counts = counts_for(&stages).ok_or(Skip::NotBillable)?;
    let timing = bytes_field(chat_model, CM_TIMING).unwrap_or_default();
    let generation = Generation {
        idx,
        // `#19` verbatim: the `#21` display label is server-supplied, renamed
        // across releases and possibly localized, so pricing it would bill a
        // model the source never named. A routing label (`gemini-default`) is
        // left exactly as it is for the same reason — it names a router, not a
        // model, and prices as unpriced rather than as a guess.
        model: text_field(chat_model, CM_MODEL_ID),
        counts,
        stages,
        response_id: text_field(usage, U_RESPONSE_ID),
        own_timestamp: explicit_timestamp(timing),
        inferred_timestamp: inferred_timestamp_ms(timing, lifetime),
    };
    let retries = decode_retries(idx, chat_model, &generation);
    Ok(Decoded { generation, retries })
}

/// The `#17` attempts that were **separate calls**: an item is a generation only
/// when its usage carries a responseId that the parent's is not.
///
/// The rest of what the box holds is a restatement of `#4` — 81,260 of this
/// machine's 81,727 items repeat the parent's id with identical stages, and the
/// 435 that carry no id at all hold all-zero usage — so billing every item would
/// double a turn on 99.4 % of rows to gain 17 calls. Attribution follows the
/// response it belongs to: the attempt box writes no `#19` model and no stamp of
/// its own, so the model and timestamp come from the generation that carries them.
fn decode_retries(idx: i64, chat_model: &[u8], parent: &Generation) -> Vec<Generation> {
    let mut out = Vec::new();
    for item in bytes_fields(chat_model, CM_RETRIES) {
        let Some(usage) = bytes_field(item, RI_USAGE) else { continue };
        let Some(id) = text_field(usage, U_RESPONSE_ID) else { continue };
        if parent.response_id.as_deref() == Some(id.as_str()) {
            continue;
        }
        let Some(stages) = read_stages(usage) else { continue };
        let Some(counts) = counts_for(&stages) else { continue };
        out.push(Generation {
            idx,
            model: parent.model.clone(),
            counts,
            stages,
            response_id: Some(id),
            own_timestamp: parent.own_timestamp,
            inferred_timestamp: parent.inferred_timestamp,
        });
    }
    out
}

/// The six usage stages, or `None` when a billed stage decodes past
/// [`IMPLAUSIBLE_TOKENS`] — which is a moved field number, not a big turn.
///
/// An empty `usage` message is legitimate (agy writes one for a preamble turn)
/// and decodes to all-zero stages, which [`counts_for`] then declines; the gate
/// here therefore ignores `#4.1`, whose constant value carries no information.
fn read_stages(usage: &[u8]) -> Option<Stages> {
    let raw = |field: u32| varint_field(usage, field).unwrap_or(0);
    let stages = Stages {
        prefix: raw(U_PREFIX),
        input: raw(U_INPUT),
        total_output: raw(U_OUTPUT_TOTAL),
        cache_read: raw(U_CACHE_READ),
        out_a: raw(U_OUT_A),
        out_b: raw(U_OUT_B),
    };
    let plausible = [stages.input, stages.total_output, stages.cache_read, stages.out_a, stages.out_b]
        .into_iter()
        .all(|value| value < IMPLAUSIBLE_TOKENS);
    plausible.then_some(stages)
}

/// The stage mapping, and the whole value of this adapter. `None` for a turn with
/// nothing to bill.
pub(crate) fn counts_for(stages: &Stages) -> Option<TokenCounts> {
    let counts = TokenCounts {
        // `#4.2` is the *uncached* remainder of the prompt. `#4.1` — a constant
        // 1 071 here, identical to `#1.3`, i.e. the agent's fixed system-and-tools
        // prefix — is deliberately not added: tokscale and TokenTracker read it as
        // billable prompt, but every decoder that names `#4.2` calls it the
        // non-cached input, so folding the constant in would bill one prefix per
        // turn against a price line that does not exist.
        input: stages.input as f64,
        // `#4.4` = cache write is ccusage's claim alone (1 of 5 decoders), and the
        // field appears in 0 of 83 local rows and 0 of 5 480 sibling-root rows:
        // there is no stage there to fill.
        cache_creation: 0.0,
        cache_read: stages.cache_read as f64,
        // `#4.3` is what the server totalled as output and equals `#4.9 + #4.10` on
        // 78/78 local rows and 5 476/5 476 rows in the sibling roots; the sum of the
        // split is only the fallback for a build that stops writing the total.
        output: if stages.total_output > 0 {
            stages.total_output
        } else {
            stages.out_a + stages.out_b
        } as f64,
        // DISPUTED: tokscale and TokenTracker call `#4.10` the reasoning half,
        // ccusage and 1yayaye name `#4.9` instead. `reasoning` is a sub-breakdown of
        // `output` and is never added on top, so whichever label is right every
        // total and every price stays identical; this side reports `#4.10`.
        reasoning: stages.out_b as f64,
        // The blobs carry `cost_summary` / `credit_usage_summary` numbers too.
        // Ignored on purpose: money is computed centrally from the shared price
        // table, which is what keeps every source comparable.
        credits: 0.0,
    };
    (!counts.is_zero()).then_some(counts)
}

/// A trimmed, non-empty string field, or `None`. A field holding binary is
/// absent rather than lossy-decoded, so a payload can never masquerade as a
/// model id.
fn text_field(buf: &[u8], field: u32) -> Option<String> {
    let text = string_field(buf, field)?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `#9.#4`: the explicitly typed Timestamp agy ≤ 1.1.17 writes. Trusted with no
/// corroboration, because it is a real Timestamp read off real databases rather
/// than an inference about unknown bytes.
fn explicit_timestamp(timing: &[u8]) -> Option<i64> {
    proto_timestamp_ms(bytes_field(timing, T_TIMESTAMP)?).filter(|ms| *ms > 0)
}

/// A protobuf `{#1: seconds, #2: nanos}` Timestamp, in epoch milliseconds.
pub(crate) fn proto_timestamp_ms(ts: &[u8]) -> Option<i64> {
    // `seconds` is an unbounded wire varint, so a torn blob can carry a value
    // whose `* 1000` overflows: checked arithmetic and `None`, never a panic.
    let seconds = i64::try_from(varint_field(ts, 1)?).ok()?;
    let nanos = i64::try_from(varint_field(ts, 2).unwrap_or(0)).ok()?;
    if !(0..=999_999_999).contains(&nanos) {
        return None;
    }
    seconds.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

/// The epoch window a *guessed* timestamp has to fall inside to be believed: the
/// owning conversation's own lifetime. Eight bytes of unconfirmed meaning cover a
/// few percent of the `u64` range as a plausible absolute date, which is far too
/// loose to date a turn on; bounded to a span usually hours wide, a wrong reading
/// stops being actionable. The trade is deliberate: rejecting a real stamp only
/// restores the conservative fallback dating, while accepting a wrong one
/// silently corrupts day buckets with no correction path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lifetime {
    pub start_ms: i64,
    pub end_ms: i64,
}

impl Lifetime {
    /// An hour of tolerance each side: the created-at and the turn stamp come off
    /// one clock, so that much absorbs a clock adjustment or a database copied
    /// from a machine running ahead, and nothing more.
    pub(crate) fn new(created_ms: Option<i64>, read_ms: i64) -> Option<Self> {
        let created = created_ms.filter(|ms| *ms > 0)?;
        Some(Self {
            start_ms: created - 3_600_000,
            end_ms: created.max(read_ms) + 3_600_000,
        })
    }

    fn holds(&self, ms: i64) -> bool {
        (self.start_ms..=self.end_ms).contains(&ms)
    }
}

/// The 1.1.18 fallback. `#9.#2` says "unset" (`int64` -1, which reaches this
/// reader as `u64::MAX`) and `#9.#10` holds bytes whose layout no source
/// publishes, so a candidate is accepted only when it is shaped like a Timestamp
/// *and* lands inside [`Lifetime`].
///
/// Deliberately not attempted: tokscale's raw eight-byte endianness contest. On
/// this machine `#9.#10` is a well-formed `{#1, #4}` cache message (208 / 256 000,
/// 30 254 / 256 000 — a cached-token count beside a context limit), so guessing
/// over eight raw bytes would only ever manufacture dates out of non-time fields.
pub(crate) fn inferred_timestamp_ms(timing: &[u8], lifetime: Option<Lifetime>) -> Option<i64> {
    // The sentinel is the only evidence that this is the new layout; without it an
    // absent `#4` means "no stamp", not "go and guess field 10".
    if varint_field(timing, T_UNSET) != Some(u64::MAX) {
        return None;
    }
    let payload = bytes_field(timing, T_OPAQUE)?;
    let lifetime = lifetime?;
    let candidate = proto_timestamp_ms(payload).or_else(|| epoch_scalar_to_ms(varint_field(payload, 1)?))?;
    lifetime.holds(candidate).then_some(candidate)
}

/// Reads a bare integer as an epoch time, its unit decided by magnitude.
///
/// `u64::MAX` is agy's "unset" marker and is rejected outright. Across the
/// plausible window the second / millisecond / microsecond / nanosecond ranges
/// are disjoint, so at most one unit yields a date and an unrelated payload (an
/// id, a hash, a token count) falls through to `None`.
pub(crate) fn epoch_scalar_to_ms(value: u64) -> Option<i64> {
    if value == u64::MAX {
        return None;
    }
    let value = i64::try_from(value).ok()?;
    // 2020-01-01 .. 2100-01-01: fixed constants, so the verdict is a property of
    // the bytes and cannot change as the calendar advances.
    const LOWER: i64 = 1_577_836_800_000;
    const UPPER: i64 = 4_102_444_800_000;
    [
        value.checked_mul(1000),
        Some(value),
        Some(value / 1_000),
        Some(value / 1_000_000),
    ]
    .into_iter()
    .flatten()
    .find(|ms| (LOWER..=UPPER).contains(ms))
}

/// One `steps` row of type 15, i.e. a model turn, reduced to what dates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepStamp {
    pub ts_ms: i64,
    pub response_id: Option<String>,
    pub gen_idx: Option<i64>,
}

/// `steps.metadata` → the same turn's wall-clock stamp, which is what recovers a
/// timestamp for the agy versions that stopped writing one into `gen_metadata`.
pub(crate) fn step_stamp(metadata: &[u8]) -> Option<StepStamp> {
    let ts_ms = proto_timestamp_ms(bytes_field(metadata, S_TIMESTAMP)?)?;
    let response_id = bytes_field(metadata, S_USAGE).and_then(|usage| text_field(usage, U_RESPONSE_ID));
    let gen_idx = bytes_field(metadata, S_GEN_REF)
        .and_then(|group| varint_field(group, G_GEN_IDX))
        .and_then(|v| i64::try_from(v).ok());
    Some(StepStamp { ts_ms, response_id, gen_idx })
}

/// Conversation-level facts, shared by every row in one database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Conversation {
    pub created_ms: Option<i64>,
    pub project: Option<String>,
}

/// `trajectory_metadata_blob.data` → the created-at that dates the conversation
/// and the workspace label every event in it inherits.
pub(crate) fn conversation_meta(blob: &[u8]) -> Conversation {
    Conversation {
        created_ms: bytes_field(blob, B_CREATED).and_then(proto_timestamp_ms).filter(|ms| *ms > 0),
        project: bytes_field(blob, B_FOLDER)
            .and_then(|folder| string_field(folder, B_FOLDER_URI))
            .and_then(file_uri_to_path),
    }
}

/// `file:///a/b` → `/a/b`, `file:///C:/x` → `C:/x`. Percent-escapes are decoded
/// because a workspace path may carry a space or a non-ASCII directory name.
fn file_uri_to_path(uri: &str) -> Option<String> {
    let decoded = percent_decode(uri.strip_prefix("file://")?);
    let bytes = decoded.as_bytes();
    // `file:///` leaves one empty leading segment; a Windows drive letter needs
    // that slash dropped, a POSIX path keeps it.
    Some(if bytes.first() == Some(&b'/') && bytes.len() > 2 && bytes[2] == b':' {
        decoded[1..].to_string()
    } else {
        decoded
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Byte offset just past `field` in `buf`, for the diagnostics on [`Skip`].
fn offset_of(buf: &[u8], field: u32) -> usize {
    let mut r = Reader::new(buf);
    while let Some((n, _)) = r.next() {
        if n == field {
            return r.pos();
        }
    }
    buf.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{tag_bytes, tag_fixed64, tag_varint};

    const CM_MODEL_LABEL: u32 = 21;
    const U_CACHE_WRITE: u32 = 4;

    /// A `usage` message in the exact field order the real rows use.
    fn usage(input: u64, cache_read: u64, out_a: u64, out_b: u64, response: &str) -> Vec<u8> {
        let mut u = tag_varint(U_PREFIX, 1071);
        u.extend(tag_varint(U_INPUT, input));
        u.extend(tag_varint(U_OUTPUT_TOTAL, out_a + out_b));
        if cache_read > 0 {
            u.extend(tag_varint(U_CACHE_READ, cache_read));
        }
        u.extend(tag_varint(U_CACHE_WRITE, 4096));
        u.extend(tag_varint(U_OUT_A, out_a));
        u.extend(tag_varint(U_OUT_B, out_b));
        u.extend(tag_bytes(U_RESPONSE_ID, response.as_bytes()));
        u
    }

    fn chat_model(usage: &[u8], timing: &[u8], model: Option<&str>) -> Vec<u8> {
        let mut cm = tag_varint(3, 1071);
        cm.extend(tag_bytes(CM_USAGE, usage));
        if !timing.is_empty() {
            cm.extend(tag_bytes(CM_TIMING, timing));
        }
        if let Some(m) = model {
            cm.extend(tag_bytes(CM_MODEL_ID, m.as_bytes()));
        }
        cm.extend(tag_bytes(CM_MODEL_LABEL, b"Gemini 3.6 Flash (High)"));
        cm
    }

    fn blob(chat_model: &[u8]) -> Vec<u8> {
        tag_bytes(F_CHAT_MODEL, chat_model)
    }

    fn gen(idx: i64, chat_model: &[u8]) -> Generation {
        decode_generation(idx, &blob(chat_model), None).expect("decodes").generation
    }

    fn decoded(idx: i64, chat_model: &[u8]) -> Decoded {
        decode_generation(idx, &blob(chat_model), None).expect("decodes")
    }

    /// One `#17` item: a RetryInfo whose `#2` is the attempt's usage message.
    fn retry(usage: &[u8]) -> Vec<u8> {
        tag_bytes(RI_USAGE, usage)
    }

    /// The same turn, its `#4`, and whatever `#17` boxes are attached.
    fn chat_model_retok(cm_usage: &[u8], retries: &[Vec<u8>]) -> Vec<u8> {
        let mut cm = chat_model(cm_usage, &[], Some("gemini-3.6-flash"));
        for item in retries {
            cm.extend(tag_bytes(CM_RETRIES, item));
        }
        cm
    }

    #[test]
    fn the_output_split_never_reaches_the_total_twice() {
        let g = gen(0, &chat_model(&usage(5000, 16000, 162, 48, "r1"), &[], Some("gemini-3.6-flash")));
        assert_eq!((g.counts.input, g.counts.cache_read), (5000.0, 16000.0));
        assert_eq!(g.stages.total_output, g.stages.out_a + g.stages.out_b, "#4.3 == #4.9 + #4.10");
        assert_eq!(g.counts.output, 210.0);
        assert_eq!(g.counts.reasoning, 48.0, "informational sub-split only");
        assert_eq!(g.counts.total(), 5000.0 + 16000.0 + 210.0);
        assert_eq!(g.model.as_deref(), Some("gemini-3.6-flash"));
        assert_eq!(g.response_id.as_deref(), Some("r1"));
    }

    #[test]
    fn the_prefix_constant_is_not_billed_and_cache_write_stays_zero() {
        // `#4.1` is on the wire and `#4.4` is planted here, yet neither becomes a
        // stage: one is an unpriceable fixed prefix, the other a 1-of-5 claim.
        let g = gen(1, &chat_model(&usage(10, 0, 3, 2, "r2"), &[], None));
        assert_eq!(g.stages.prefix, 1071, "read off the wire, deliberately unbilled");
        assert_eq!(
            (g.counts.input, g.counts.cache_creation, g.counts.cache_read),
            (10.0, 0.0, 0.0)
        );
        assert_eq!(g.counts.output, 5.0);
        assert_eq!(g.model, None, "no #19 gives no model, never the display label");
    }

    #[test]
    fn total_output_falls_back_to_the_split_when_the_server_stops_writing_it() {
        let mut u = tag_varint(U_INPUT, 7);
        u.extend(tag_varint(U_OUT_A, 4));
        u.extend(tag_varint(U_OUT_B, 5));
        u.extend(tag_bytes(U_RESPONSE_ID, b"r3"));
        let g = gen(2, &chat_model(&u, &[], Some("m")));
        assert_eq!((g.counts.output, g.stages.total_output), (9.0, 0));
    }

    #[test]
    fn an_empty_or_zero_usage_message_is_not_a_billable_event() {
        assert_eq!(decode_generation(0, &blob(&tag_bytes(CM_USAGE, &[])), None), Err(Skip::NotBillable));
        assert_eq!(decode_generation(0, &blob(&[]), None), Err(Skip::NotBillable));
        assert_eq!(
            decode_generation(0, &blob(&chat_model(&usage(0, 0, 0, 0, "r"), &[], Some("m"))), None),
            Err(Skip::NotBillable),
            "a turn that reported nothing has nothing to bill"
        );
    }

    #[test]
    fn a_row_without_a_chat_model_is_not_a_generation() {
        assert_eq!(decode_generation(0, &tag_varint(2, 7), None), Err(Skip::NoChatModel));
        assert_eq!(decode_generation(0, &[], None), Err(Skip::NoChatModel));
    }

    #[test]
    fn an_absurd_stage_is_reported_as_a_moved_field_not_as_a_bill() {
        let mut u = tag_varint(U_PREFIX, 1071);
        u.extend(tag_varint(U_INPUT, IMPLAUSIBLE_TOKENS));
        u.extend(tag_varint(U_OUTPUT_TOTAL, 5));
        let err = decode_generation(3, &blob(&tag_bytes(CM_USAGE, &u)), None).unwrap_err();
        assert!(matches!(err, Skip::OutOfRange(pos) if pos > 0), "{err:?}");
        // 3 bytes of `#4.1` (tag + a 1071 varint), then `#4.2`'s tag and its
        // 6-byte 1e12: the offset sits just past the stage that cannot be real.
        assert_eq!(offset_of(&u, U_INPUT), 3 + 7);
    }

    #[test]
    fn a_truncated_blob_is_dropped_with_the_offset_where_it_stopped() {
        let mut cm = chat_model(&usage(1, 0, 1, 0, "r"), &[], Some("m"));
        cm.extend([0x42, 0x7f]); // field 8, "127 bytes follow": nothing does
        let cm_break = cm.len() - 2;
        assert_eq!(decode_generation(4, &blob(&cm), None), Err(Skip::Truncated(cm_break)));
        let mut tail = tag_bytes(CM_USAGE, &usage(1, 0, 1, 0, "r"));
        tail.extend([0x12, 0x10]);
        assert_eq!(decode_generation(4, &blob(&tail), None), Err(Skip::Truncated(tail.len() - 2)));
    }

    #[test]
    fn garbage_blobs_are_skipped_and_never_panic() {
        for n in 0..96u8 {
            let buf: Vec<u8> = (0..n).map(|i| i.wrapping_mul(251).wrapping_add(3)).collect();
            let _ = decode_generation(0, &buf, None);
            let _ = step_stamp(&buf);
            let _ = conversation_meta(&buf);
            let _ = counts_for(&Stages::default());
            let _ = inferred_timestamp_ms(&buf, Lifetime::new(Some(1_786_402_449_758), 1_786_500_000_000));
        }
    }

    #[test]
    fn the_timestamp_dialects_and_the_unset_sentinel() {
        let mut timing = tag_varint(T_UNSET, u64::MAX);
        let mut stamp = tag_varint(1, 1_786_402_450);
        stamp.extend(tag_varint(2, 80_620_000));
        timing.extend(tag_bytes(T_TIMESTAMP, &stamp));
        assert_eq!(explicit_timestamp(&timing), Some(1_786_402_450_080));

        // The 1.1.18 shape on this machine: sentinel present, no `#4`, and a `#10`
        // holding cache metadata rather than time. It must not become a date.
        let opaque = {
            let mut t = tag_varint(T_UNSET, u64::MAX);
            let mut p = tag_varint(1, 30_254);
            p.extend(tag_varint(4, 256_000));
            t.extend(tag_bytes(T_OPAQUE, &p));
            t
        };
        let lifetime = Lifetime::new(Some(1_786_402_449_758), 1_786_500_000_000).unwrap();
        assert_eq!(explicit_timestamp(&opaque), None);
        assert_eq!(inferred_timestamp_ms(&opaque, Some(lifetime)), None);
        assert_eq!(inferred_timestamp_ms(&opaque, None), None, "no anchor, no guess");
    }

    #[test]
    fn an_inferred_stamp_is_accepted_only_inside_the_owners_lifetime() {
        let within = {
            let mut t = tag_varint(T_UNSET, u64::MAX);
            t.extend(tag_bytes(T_OPAQUE, &tag_varint(1, 1_786_402_460)));
            t
        };
        let outside = {
            let mut t = tag_varint(T_UNSET, u64::MAX);
            t.extend(tag_bytes(T_OPAQUE, &tag_varint(1, 1_600_000_000)));
            t
        };
        let lifetime = Lifetime::new(Some(1_786_402_449_758), 1_786_410_000_000).unwrap();
        assert_eq!(inferred_timestamp_ms(&within, Some(lifetime)), Some(1_786_402_460_000));
        assert_eq!(inferred_timestamp_ms(&outside, Some(lifetime)), None, "cannot predate its own conversation");
        // A `#9` without the sentinel is the old layout: never guess over it.
        assert_eq!(
            inferred_timestamp_ms(&tag_bytes(T_OPAQUE, &tag_varint(1, 1_786_402_460)), Some(lifetime)),
            None
        );
        assert_eq!(Lifetime::new(Some(0), 1), None, "a zero created-at is no anchor");
        assert_eq!(Lifetime::new(None, 1), None);
    }

    #[test]
    fn epoch_unit_detection_reads_one_int_as_one_date() {
        assert_eq!(epoch_scalar_to_ms(1_786_402_450), Some(1_786_402_450_000));
        assert_eq!(epoch_scalar_to_ms(1_786_402_450_123), Some(1_786_402_450_123));
        assert_eq!(epoch_scalar_to_ms(1_786_402_450_123_456), Some(1_786_402_450_123));
        assert_eq!(epoch_scalar_to_ms(1_786_402_450_123_456_789), Some(1_786_402_450_123));
        assert_eq!(epoch_scalar_to_ms(u64::MAX), None, "the unset sentinel");
        assert_eq!(epoch_scalar_to_ms(42), None, "an unrelated small integer");
        assert_eq!(epoch_scalar_to_ms(30_254), None, "a cached-token count is not a date");
    }

    #[test]
    fn malformed_timestamp_messages_degrade_to_none_not_to_an_overflow_panic() {
        assert_eq!(proto_timestamp_ms(&tag_varint(1, u64::MAX)), None);
        assert_eq!(proto_timestamp_ms(&tag_varint(1, 1_786_402_450)), Some(1_786_402_450_000));
        let mut nanos = tag_varint(1, 1_786_402_450);
        nanos.extend(tag_varint(2, 1_000_000_000)); // outside the Timestamp spec
        assert_eq!(proto_timestamp_ms(&nanos), None);
        assert_eq!(proto_timestamp_ms(&[]), None);
        assert_eq!(proto_timestamp_ms(&tag_fixed64(1, 5)), None, "wrong wire type is not a second count");
        assert_eq!(explicit_timestamp(&tag_fixed64(T_TIMESTAMP, 1_786_402_450)), None);
    }

    #[test]
    fn a_step_row_reveals_its_stamp_response_id_and_gen_index() {
        let mut meta = tag_bytes(S_TIMESTAMP, &tag_varint(1, 1_786_402_450));
        meta.extend(tag_bytes(S_USAGE, &tag_bytes(U_RESPONSE_ID, b"resp-7")));
        let mut g = tag_varint(1, 7);
        g.extend(tag_varint(G_GEN_IDX, 41));
        meta.extend(tag_bytes(S_GEN_REF, &g));
        assert_eq!(
            step_stamp(&meta),
            Some(StepStamp { ts_ms: 1_786_402_450_000, response_id: Some("resp-7".into()), gen_idx: Some(41) })
        );
        // A step that carries no gen index still dates itself.
        assert_eq!(step_stamp(&tag_bytes(S_TIMESTAMP, &tag_varint(1, 100))).map(|s| s.gen_idx), Some(None));
        assert_eq!(step_stamp(&tag_varint(1, 100)), None, "a varint where a Timestamp must be");
        assert_eq!(step_stamp(&[]), None);
    }

    #[test]
    fn conversation_meta_reads_the_created_at_and_the_workspace() {
        let mut blob = tag_bytes(B_FOLDER, &tag_bytes(B_FOLDER_URI, b"file:///Users/demo/proj"));
        let mut created = tag_varint(1, 1_786_402_449);
        created.extend(tag_varint(2, 758_423_000));
        blob.extend(tag_bytes(B_CREATED, &created));
        let meta = conversation_meta(&blob);
        assert_eq!(meta.created_ms, Some(1_786_402_449_758));
        assert_eq!(meta.project.as_deref(), Some("/Users/demo/proj"));
        assert_eq!(conversation_meta(&[]), Conversation::default());
        assert_eq!(
            conversation_meta(&tag_bytes(B_FOLDER, &tag_bytes(B_FOLDER_URI, b"file:///C:/Users/demo/p")))
                .project
                .as_deref(),
            Some("C:/Users/demo/p")
        );
        assert_eq!(
            conversation_meta(&tag_bytes(
                B_FOLDER,
                &tag_bytes(B_FOLDER_URI, b"file:///Users/demo/a%20b/%E4%B8%AD")
            ))
            .project
            .as_deref(),
            Some("/Users/demo/a b/中")
        );
        assert_eq!(conversation_meta(&tag_bytes(B_FOLDER, &tag_varint(B_FOLDER_URI, 5))).project, None);
    }

    #[test]
    fn merging_streamed_partials_takes_the_largest_snapshot_never_the_sum() {
        let small = Stages { prefix: 1071, input: 100, total_output: 20, cache_read: 50, out_a: 15, out_b: 5 };
        let big = Stages { prefix: 1071, input: 140, total_output: 34, cache_read: 50, out_a: 24, out_b: 10 };
        let merged = small.max_each(big);
        assert_eq!(merged, big);
        assert_eq!(counts_for(&merged).unwrap().total(), 140.0 + 50.0 + 34.0);
        assert_eq!(big.max_each(small), merged, "order does not matter");
        // Replaying a partial that grew nothing cannot inflate a turn either.
        assert_eq!(counts_for(&merged.max_each(small)).unwrap().output, 34.0);
    }

    /// 81,260 of the 81,727 `#17` boxes on this machine restate `#4` verbatim
    /// under the same responseId. Billing them is the double count.
    #[test]
    fn a_retry_box_that_repeats_the_response_is_not_a_second_call() {
        let u = usage(5000, 16000, 162, 48, "r1");
        let d = decoded(7, &chat_model_retok(&u, &[retry(&u)]));
        assert_eq!(d.generation.response_id.as_deref(), Some("r1"));
        assert_eq!(d.retries.len(), 0, "the copy is the same call, not a new one");
    }

    /// Same shape, different numbers, still the parent's id: the failed attempt
    /// of one response, which the server's own total already stands as `#4`.
    #[test]
    fn a_failed_attempt_under_the_parents_id_does_not_add_a_turn() {
        let d = decoded(
            65,
            &chat_model_retok(
                &usage(7454, 186932, 400, 96, "HFd6av70O7ClmtkPoKzIQA"),
                &[retry(&usage(3589, 93470, 50, 48, "HFd6av70O7ClmtkPoKzIQA"))],
            ),
        );
        assert_eq!(d.retries.len(), 0, "one responseId means one call");
    }

    /// A box with no responseId is an id-less zero on every local row (435 of
    /// them), and without an identity there is no way to tell it from a copy —
    /// so it is not billed.
    #[test]
    fn an_id_less_retry_box_is_not_billed_as_a_call() {
        let mut u = tag_varint(U_INPUT, 900);
        u.extend(tag_varint(U_OUTPUT_TOTAL, 90));
        let d = decoded(3, &chat_model_retok(&usage(5000, 16000, 162, 48, "r1"), &[retry(&u)]));
        assert_eq!(d.retries.len(), 0);
    }

    /// The 17 rows this fix is for: an attempt that carries its own responseId is
    /// a separate API call and must be billed, wearing the parent's model and
    /// stamp because the attempt box writes neither.
    #[test]
    fn a_retry_with_its_own_response_id_is_billed_as_its_own_call() {
        let timing = {
            let mut t = tag_varint(T_UNSET, u64::MAX);
            t.extend(tag_bytes(T_TIMESTAMP, &tag_varint(1, 1_786_402_450)));
            t
        };
        let mut cm = chat_model(&usage(7454, 186932, 400, 96, "r-main"), &timing, Some("gemini-3.6-flash"));
        cm.extend(tag_bytes(CM_RETRIES, &retry(&usage(3865, 93462, 350, 48, "r-retry"))));
        let d = decode_generation(65, &blob(&cm), None).expect("decodes");
        assert_eq!(d.retries.len(), 1, "the distinct id is a distinct call");
        let r = &d.retries[0];
        assert_eq!(r.response_id.as_deref(), Some("r-retry"));
        assert_eq!((r.counts.input, r.counts.cache_read, r.counts.output), (3865.0, 93462.0, 398.0));
        assert_eq!(r.model.as_deref(), Some("gemini-3.6-flash"), "attributed to the response it belongs to");
        assert_eq!(r.idx, 65, "the same row");
        assert_eq!(r.own_timestamp, d.generation.own_timestamp);
    }

    /// Two attempts of their own on one row: both billed, none collapsed into the
    /// parent — which is what makes the window key, not the row, the identity.
    #[test]
    fn several_distinct_attempts_are_several_calls() {
        let u = usage(1000, 8000, 100, 20, "r-main");
        let d = decoded(
            9,
            &chat_model_retok(
                &u,
                &[retry(&usage(200, 1000, 20, 5, "r-a")), retry(&usage(300, 2000, 30, 7, "r-b"))],
            ),
        );
        assert_eq!(d.retries.len(), 2);
        assert_eq!(d.generation.counts.total(), 1000.0 + 8000.0 + 120.0);
        assert_eq!(
            d.retries.iter().map(|r| r.counts.total()).collect::<Vec<_>>(),
            vec![1225.0, 2337.0],
            "each attempt is counted once, at its own size"
        );
    }
}
