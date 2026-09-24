//! One module per vendor whose quota lives outside its logs.

mod antigravity;
mod claude;
mod cline;
mod codex;
mod copilot;
mod gemini;
mod opencode;
mod qoder;
mod ccswitch;
mod zcode;

pub use antigravity::AntigravityQuota;
pub use claude::ClaudeQuota;
pub use cline::ClineQuota;
pub use codex::CodexQuota;
pub use copilot::CopilotQuota;
pub use gemini::GeminiQuota;
pub use opencode::OpenCodeQuota;
pub use qoder::QoderQuota;
pub use ccswitch::CcSwitchQuota;
pub use zcode::ZcodeQuota;

use crate::QuotaProbe;

/// Probes whose credential is optional on this machine: they cost one stat call
/// when the tool is not configured, and nothing else.
pub fn optional() -> Vec<Box<dyn QuotaProbe>> {
    vec![
        Box::new(AntigravityQuota),
        Box::new(ClineQuota),
        Box::new(OpenCodeQuota),
        Box::new(CodexQuota),
        Box::new(CopilotQuota),
        Box::new(GeminiQuota),
        Box::new(QoderQuota),
        Box::new(CcSwitchQuota),
        Box::new(ZcodeQuota),
    ]
}
