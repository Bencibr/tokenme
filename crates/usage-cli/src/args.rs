//! clap surface for `tokenme`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "tokenme",
    about = "AI token usage and cost, straight from the tools' own logs",
    long_about = "tokenme reads Claude Code, Codex and OpenCode logs incrementally into a local \
                  SQLite index and renders cost/token reports from it.\n\n\
                  Money and period math lives in usage-core, so this CLI and the menu-bar panel \
                  can never disagree about a number.",
    version
)]
pub struct Cli {
    #[command(flatten)]
    pub g: Global,

    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Args, Clone, Debug)]
pub struct Global {
    /// Only ingest/report these sources; repeatable (claude, codex, opencode)
    #[arg(long, value_name = "ID", global = true)]
    pub tool: Vec<String>,

    /// Machine-readable output on stdout
    #[arg(long, global = true)]
    pub json: bool,

    /// Never touch the network; use the cached/bundled price table
    #[arg(long, global = true)]
    pub offline: bool,

    /// Force a price, e.g. glm-5.2=2/8/2.5/0.4 (USD per 1M tokens); repeatable
    #[arg(long = "pricing-override", value_name = "MODEL=IN/OUT/CC/CR", global = true)]
    pub pricing_override: Vec<String>,

    /// Ignore events before this date
    #[arg(long, value_name = "YYYY-MM-DD", global = true)]
    pub since: Option<String>,

    /// Ignore events after this date (inclusive), and report as of it
    #[arg(long, value_name = "YYYY-MM-DD", global = true)]
    pub until: Option<String>,

    /// Index database path
    #[arg(long, value_name = "PATH", global = true)]
    pub db: Option<PathBuf>,

    /// Report from the existing index without ingesting first
    #[arg(long, global = true)]
    pub no_ingest: bool,

    /// Suppress the stderr progress lines
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Explain what was ingested, on stderr
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// JSON diagnostics grant/configuration file for the remote MCP stdio
    /// bridge. Without this file diagnostics-stdio serves the local standard
    /// MCP JSON-RPC protocol over stdin/stdout.
    #[arg(long, value_name = "PATH", global = true)]
    pub diagnostics_config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Which sources exist here, and what has been indexed
    Detect,

    /// Per-day totals
    Daily {
        #[arg(long, value_name = "N", default_value_t = 14)]
        days: i64,
    },

    /// Per-week totals
    Weekly {
        #[arg(long, value_name = "N", default_value_t = 12)]
        weeks: i64,
    },

    /// Per-month totals
    Monthly {
        #[arg(long, value_name = "N", default_value_t = 12)]
        months: i64,
    },

    /// One window, broken down, with the delta against the previous slice
    Report {
        #[arg(long, value_enum, default_value_t = Win::Day)]
        window: Win,

        #[arg(long, value_enum, default_value_t = Group::Tool)]
        group: Group,
    },

    /// Recent sessions
    Sessions {
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
    },

    /// Newest quota sample each source reported
    Quota,

    /// DSH ledger-vs-index audit: the app's own numbers next to what got indexed
    DshDoctor,

    /// Codex replay-vs-index audit: every rollout re-parsed from scratch,
    /// next to what got indexed
    CodexDoctor,

    /// Log in to WorkBuddy (browser SSO once; the token feeds the quota probe)
    WorkbuddyLogin,

    /// Where a price came from: models.dev sells most models through several
    /// providers, so a cost number is only meaningful together with the listing
    /// that won the precedence rule.
    Pricing {
        #[command(subcommand)]
        action: PricingCmd,
    },

    /// Spend caps tokenme measures against its own cost, for the tools whose
    /// vendor publishes no limit (Cline, ZCode, …)
    Budget {
        #[command(subcommand)]
        action: BudgetCmd,
    },

    /// The macOS application icon each tool row would show, as a data URL. What
    /// the panel asks for over IPC; written to `icons.json` to review the panel
    /// in a browser.
    Icons,

    /// Serve the diagnostics MCP protocol over newline-delimited JSON-RPC
    /// stdio. This is intended for a local diagnostics host; stdout is
    /// reserved for protocol responses and operational errors go to stderr.
    DiagnosticsStdio,

    /// Drive the index explicitly
    Index {
        /// Drop the index and re-read the retention window
        #[arg(long)]
        rebuild: bool,

        /// Drop events older than the retention window
        #[arg(long)]
        prune: bool,

        /// Show index state without ingesting
        #[arg(long)]
        status: bool,

        /// Take the ingest lease even if another tokenme process holds it
        #[arg(long)]
        force: bool,
    },

    /// Export a window of this machine's events as a sync bundle (JSONL.gz +
    /// manifest) for another machine to import. Ingests first, then writes to
    /// `~/tokenme-sync` by default, which is where the menu-bar engine picks
    /// bundles up. A quiet or logless machine still ships a valid (empty)
    /// bundle — a collector's timer must never fail on a slow day.
    Export {
        /// Window length in days, ending now
        #[arg(long, value_name = "N", default_value_t = 30)]
        days: i64,

        /// Directory the bundle lands in (default: ~/tokenme-sync)
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,

        /// Origin name recorded in the manifest and used for row identity
        /// (default: this machine's hostname)
        #[arg(long, value_name = "NAME")]
        origin: Option<String>,
    },

    /// Merge one sync bundle exported by another machine into the local index
    Import {
        /// The tokenme-*.jsonl.gz to merge; its manifest must sit next to it
        file: PathBuf,

        /// Parse, merge and reconcile, then roll back without committing
        #[arg(long)]
        dry_run: bool,
    },

    /// Build and upload one strict device-sync snapshot over HTTPS/TLS 1.3.
    /// The primary-issued stream receipt and certificate pin are mandatory.
    Sync {
        #[command(subcommand)]
        action: SyncCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum SyncCmd {
    /// Show registered devices and pending pairing requests without writing.
    Status,

    /// Run the paired secondary rollup-sync runtime in the foreground.
    Run,

    /// Stop the user-level secondary runtime through its private local control channel.
    Stop,

    /// Forget this secondary's rollup sync progress, then re-sync from
    /// scratch. Local usage history and the pairing are kept. Stop the
    /// resident runtime first.
    Reset,

    /// Create a one-time invite on the primary.
    Invite {
        #[arg(long, value_name = "URL")]
        endpoint: String,
        /// Certificate pin. When omitted it is derived from the primary's
        /// private certificate, so the copied invite is self-contained.
        #[arg(long, value_name = "HEX")]
        pin_sha256: Option<String>,
        /// PEM certificate. Defaults to the user-private primary service path.
        #[arg(long, value_name = "PATH")]
        certificate: Option<PathBuf>,
        #[arg(long, value_name = "NAME", default_value = "localhost")]
        server_name: String,
        #[arg(long, value_name = "MILLISECONDS")]
        expires_at_ms: i64,
        #[arg(long, value_name = "ID")]
        primary_id: String,
    },

    /// Submit a pairing request to the primary using an invite secret.
    Pair {
        /// A complete tokenme://device-pair invite. This is the preferred
        /// path; it persists the endpoint, pin, identity and primary binding.
        #[arg(long, value_name = "URL")]
        invite: Option<String>,
        #[arg(long, value_name = "URL")]
        endpoint: Option<String>,
        #[arg(long, value_name = "NAME")]
        server_name: Option<String>,
        #[arg(long, value_name = "PATH")]
        trust_cert: Option<PathBuf>,
        #[arg(long, value_name = "HEX")]
        pin_sha256: Option<String>,
        #[arg(long, value_name = "ID")]
        invite_id: Option<String>,
        #[arg(long, value_name = "HEX")]
        invite_secret_hex: Option<String>,
        #[arg(long, value_name = "PATH")]
        identity: Option<PathBuf>,
        #[arg(long, value_name = "NAME")]
        origin: Option<String>,
        #[arg(long, value_name = "OS")]
        os: Option<String>,
    },

    /// Approve a pairing request received by the primary.
    Approve {
        #[arg(long, value_name = "ID")]
        request_id: String,
        #[arg(long, value_name = "ID")]
        primary_id: String,
    },

    /// Run the primary HTTPS/TLS 1.3 service.
    Serve {
        #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:47821")]
        bind: String,
        #[arg(long, value_name = "PATH")]
        cert: Option<PathBuf>,
        #[arg(long, value_name = "PATH")]
        key: Option<PathBuf>,
        #[arg(long, value_name = "ID")]
        primary_id: String,
        #[arg(long)]
        allow_lan: bool,
    },

    /// Upload the selected local window to an already-approved primary.
    Upload {
        /// HTTPS endpoint, for example https://192.168.1.20:9443
        #[arg(long, value_name = "URL")]
        endpoint: String,

        /// TLS server name used for certificate validation.
        #[arg(long, value_name = "NAME")]
        server_name: String,

        /// PEM certificate used as the trust anchor for the pinned server.
        #[arg(long, value_name = "PATH")]
        trust_cert: PathBuf,

        /// SHA-256 of the server leaf certificate, exactly 64 hex digits.
        #[arg(long, value_name = "HEX")]
        pin_sha256: String,

        /// Persistent device identity JSON. Created with private permissions
        /// when absent, using --origin as its stable origin.
        #[arg(long, value_name = "PATH")]
        identity: PathBuf,

        /// Stable origin used only when creating a new identity.
        #[arg(long, value_name = "NAME")]
        origin: String,

        /// Primary id from the approved stream-open receipt.
        #[arg(long, value_name = "ID")]
        primary_id: String,

        /// Stream id from the approved stream-open receipt.
        #[arg(long, value_name = "ID")]
        stream_id: String,

        #[arg(long, value_name = "N", default_value_t = 1)]
        data_epoch: u64,

        #[arg(long, value_name = "N", default_value_t = 1)]
        generation: u64,

        /// Local window length ending at the current time.
        #[arg(long, value_name = "N", default_value_t = 30)]
        days: i64,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Win {
    Day,
    Week,
    Month,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Tool,
    Model,
    Project,
    Mcp,
    Skill,
}

#[derive(Subcommand, Debug)]
pub enum PricingCmd {
    /// Name the vendor listing that priced each MODEL, and the ones it beat
    Explain { models: Vec<String> },

    /// Every model in the index that more than one provider sells, with the
    /// money the precedence rule can move
    Contested {
        #[arg(long, value_name = "N", default_value_t = 25)]
        limit: usize,
    },
}

#[derive(Subcommand, Debug)]
pub enum BudgetCmd {
    /// Show every stored cap
    List,
    /// Set one tool's cap, e.g. `budget set zcode --daily 5 --monthly 50`
    Set {
        tool: String,

        /// USD per local day; 0 clears just this window
        #[arg(long, value_name = "USD")]
        daily: Option<f64>,

        /// USD per calendar month; 0 clears just this window
        #[arg(long, value_name = "USD")]
        monthly: Option<f64>,
    },
    /// Drop one tool's cap entirely
    Rm { tool: String },
}
