//! One module per vendor whose quota lives outside its logs.

mod agnes;
mod antigravity;
mod atomcode;
mod crypto;
mod catpaw;
mod claude;
mod cline;
mod cola;
mod codex;
mod copilot;
pub(crate) use crypto::hmac_sha256;
mod dsh;
mod funide;
mod gemini;
mod joycode;
mod kimicode;
mod minimaxcode;
mod opencode;
mod qoder;
mod trae;
mod workbuddy;
mod workbuddy_wbipc;
mod zcode;

pub use agnes::AgnesQuota;
pub use antigravity::AntigravityQuota;
pub use atomcode::AtomCodeQuota;
pub use catpaw::CatpawQuota;
pub use claude::ClaudeQuota;
pub use cline::ClineQuota;
pub use cola::ColaQuota;
pub use codex::CodexQuota;
pub use copilot::CopilotQuota;
pub use dsh::DshQuota;
pub use funide::FunIdeQuota;
pub use gemini::GeminiQuota;
pub use joycode::JoycodeQuota;
pub use kimicode::KimiCodeQuota;
pub use minimaxcode::MiniMaxCodeQuota;
pub use opencode::OpenCodeQuota;
pub use qoder::QoderQuota;
pub use trae::TraeQuota;
/// The panel's check-in button entry (see `trae::manual_checkin`).
pub fn trae_manual_checkin() -> Result<String, String> {
    trae::manual_checkin()
}
pub use workbuddy::WorkBuddyQuota;
pub use workbuddy::login as workbuddy_login;
pub use zcode::ZcodeQuota;

use crate::QuotaProbe;

/// Probes whose credential is optional on this machine: they cost one stat call
/// when the tool is not configured, and nothing else.
pub fn optional() -> Vec<Box<dyn QuotaProbe>> {
    vec![
        Box::new(AgnesQuota),
        Box::new(AntigravityQuota),
        Box::new(AtomCodeQuota),
        Box::new(ColaQuota),
        Box::new(KimiCodeQuota),
        Box::new(MiniMaxCodeQuota),
        Box::new(FunIdeQuota),
        Box::new(ClineQuota),
        Box::new(OpenCodeQuota),
        Box::new(CodexQuota),
        Box::new(CopilotQuota),
        Box::new(GeminiQuota),
        Box::new(JoycodeQuota),
        Box::new(QoderQuota),
        Box::new(WorkBuddyQuota),
        Box::new(CatpawQuota),
        Box::new(ZcodeQuota),
        Box::new(DshQuota),
        Box::new(TraeQuota { cn: false }),
        Box::new(TraeQuota { cn: true }),
    ]
}
