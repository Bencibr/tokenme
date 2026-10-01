//! UI language for the native chrome — tray menu, status-item tooltip, updater
//! copy. The webview detects the system locale itself (navigator.language) and
//! reports the verdict back through `set_ui_lang`; until it loads, and whenever
//! no webview ever runs, the default is Chinese — the product's home language.

use std::sync::atomic::{AtomicU8, Ordering};

static LANG: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// The literal for this language: `lang::get().str("今日", "Today")`.
    pub fn str(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Lang::Zh => zh,
            Lang::En => en,
        }
    }
}

pub fn set(l: Lang) {
    LANG.store(l as u8, Ordering::Relaxed);
}

pub fn get() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::Zh,
    }
}

pub fn parse(v: &str) -> Option<Lang> {
    match v {
        "en" => Some(Lang::En),
        "zh" => Some(Lang::Zh),
        _ => None,
    }
}
