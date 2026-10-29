//! Язык интерфейса: русский или английский.
//!
//! Строки задаются парами прямо в коде: `tr("Открыть", "Open")`, `trf!("{n} стр.", "{n} pages")`.
//! Выбор сохраняется в файле настроек; при первом запуске берётся язык системы.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Ru,
    En,
}

pub fn lang() -> Lang {
    if ENGLISH.load(Ordering::Relaxed) { Lang::En } else { Lang::Ru }
}

pub fn is_en() -> bool {
    lang() == Lang::En
}

/// Строка на текущем языке.
pub fn tr(ru: &'static str, en: &'static str) -> &'static str {
    if is_en() { en } else { ru }
}

/// `format!` на текущем языке: `trf!("Страниц: {n}", "Pages: {n}")`.
#[macro_export]
macro_rules! trf {
    ($ru:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_en() { format!($en $(, $arg)*) } else { format!($ru $(, $arg)*) }
    };
}

/// Файл настроек: ~/.config/pdf-tool/language (Linux), %APPDATA%\pdf-tool\language (Windows),
/// ~/Library/Application Support/pdf-tool/language (macOS).
fn settings_file() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("pdf-tool").join("language"))
}

fn parse(value: &str) -> Option<Lang> {
    match value.trim() {
        "en" => Some(Lang::En),
        "ru" => Some(Lang::Ru),
        _ => None,
    }
}

/// Язык при запуске: переменная PDF_TOOL_LANG (en/ru), затем сохранённый выбор,
/// иначе язык системы (русский — для ru, иначе английский).
pub fn init() {
    let saved = std::env::var("PDF_TOOL_LANG").ok().and_then(|v| parse(&v)).or_else(|| {
        settings_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|v| parse(&v))
    });
    let lang = saved.unwrap_or_else(|| {
        let ru = sys_locale::get_locale().is_some_and(|l| l.to_lowercase().starts_with("ru"));
        if ru { Lang::Ru } else { Lang::En }
    });
    ENGLISH.store(lang == Lang::En, Ordering::Relaxed);
}

/// Меняет язык и запоминает выбор.
pub fn set_lang(lang: Lang) {
    ENGLISH.store(lang == Lang::En, Ordering::Relaxed);
    if let Some(file) = settings_file() {
        let _ = std::fs::create_dir_all(file.parent().unwrap_or(&file));
        let _ = std::fs::write(file, if lang == Lang::En { "en" } else { "ru" });
    }
}
