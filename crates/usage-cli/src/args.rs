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
