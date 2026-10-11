//! Interface strings, shared with the window.
//!
//! Every user-facing string lives in `locales/<tag>.json` at the repository
//! root: flat `"section.name": "text"` maps, the shape Crowdin's key-value
//! JSON format reads and writes. The TypeScript window and the Rust side — the
//! tray, startup dialogs, the engine's event log — read the same files, so a
//! translation lands everywhere at once. English is the source language and
//! the fallback for any key a translation lacks; placeholders are `{name}`.
//!
//! The language is process-wide: one window, one user. Text is rendered when
//! it is produced, so an event logged before a language switch keeps the
//! language it was written in.

use std::{
    collections::HashMap,
    fmt::{Display, Write},
    sync::{Mutex, OnceLock},
};

include!(concat!(env!("OUT_DIR"), "/locales.rs"));

/// Source language of the catalogs and the answer when nothing else fits.
pub const FALLBACK: &str = "en";

/// Former Soviet republics outside the Baltics. Russian is the shared second
/// language there, so it beats English as the default for a system that does
/// not run in a language we ship.
const RUSSIAN_REGIONS: &[&str] = &[
    "RU", "BY", "UA", "MD", "GE", "AM", "AZ", "KZ", "KG", "TJ", "TM", "UZ",
];
/// The same countries by their own languages, for a Windows whose home
/// location was never set but whose interface speaks for itself.
const RUSSIAN_LANGUAGES: &[&str] = &[
    "ru", "be", "uk", "ka", "hy", "az", "kk", "ky", "tg", "tk", "uz",
];

type Catalog = HashMap<String, String>;

fn catalogs() -> &'static [(&'static str, Catalog)] {
    static CATALOGS: OnceLock<Vec<(&'static str, Catalog)>> = OnceLock::new();
    CATALOGS.get_or_init(|| {
        // A malformed file from a translation drop costs that language, never
        // the app; `every_locale_parses` keeps shipped files honest.
        LOCALES
            .iter()
            .filter_map(|(tag, json)| Some((*tag, serde_json::from_str(json).ok()?)))
            .collect()
    })
}

fn catalog(tag: &str) -> Option<&'static Catalog> {
    catalogs()
        .iter()
        .find(|(name, _)| *name == tag)
        .map(|(_, catalog)| catalog)
}

static CURRENT: Mutex<&'static str> = Mutex::new(FALLBACK);

/// Tags of every language that ships, in file-name order.
pub fn available() -> Vec<&'static str> {
    catalogs().iter().map(|(tag, _)| *tag).collect()
}

/// The shipped language for a BCP 47 tag: an exact match (`pt-BR`) first,
/// then the primary subtag (`pt` for `pt-PT`).
pub fn resolve(tag: &str) -> Option<&'static str> {
    let tag = tag.trim().replace('_', "-");
    let primary = tag.split('-').next().unwrap_or_default();
    if primary.is_empty() {
        return None;
    }
    let shipped = || catalogs().iter().map(|(name, _)| *name);
    shipped()
        .find(|name| name.eq_ignore_ascii_case(&tag))
        .or_else(|| shipped().find(|name| name.eq_ignore_ascii_case(primary)))
}

/// Switches the process language; anything unknown lands on English. Returns
/// the tag actually in effect.
pub fn set_language(tag: &str) -> &'static str {
    let resolved = resolve(tag).unwrap_or(FALLBACK);
    *CURRENT.lock().unwrap() = resolved;
    resolved
}

pub fn language() -> &'static str {
    *CURRENT.lock().unwrap()
}

/// The text for `key` in the current language.
pub fn t(key: &str) -> String {
    tf(key, &[])
}

/// The text for `key` with `{name}` placeholders filled from `args`.
pub fn tf(key: &str, args: &[(&str, &dyn Display)]) -> String {
    translate(language(), key, args)
}

/// Lookup in an explicit language — current text for the UI, a fixed one for
/// tests that must not depend on the process-wide setting.
pub fn translate(tag: &str, key: &str, args: &[(&str, &dyn Display)]) -> String {
    let template = catalog(tag)
        .and_then(|catalog| catalog.get(key))
        .or_else(|| catalog(FALLBACK).and_then(|catalog| catalog.get(key)))
        .map(String::as_str)
        // A missing key shows itself instead of an empty line; the key-usage
        // test keeps that from ever shipping.
        .unwrap_or(key);
    fill(template, args)
}

fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let name = after.find('}').map(|end| &after[..end]).filter(|name| {
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
        match name {
            Some(name) => {
                match args.iter().find(|(arg, _)| *arg == name) {
                    Some((_, value)) => {
                        let _ = write!(out, "{value}");
                    }
                    None => out.push_str(&rest[start..start + name.len() + 2]),
                }
                rest = &after[name.len() + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Placeholder names a template uses, for checking translations against the
/// source: a translator who renames `{name}` silently drops the value.
pub fn placeholders(template: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        rest = &rest[start + 1..];
        if let Some(end) = rest.find('}') {
            let name = &rest[..end];
            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                names.push(name);
                rest = &rest[end + 1..];
            }
        }
    }
    names.sort_unstable();
    names
}

/// First-run language from what Windows says about the user.
///
/// `interface` is the Windows display language (`ru-RU`, `en-US`); `regions`
/// are two-letter countries from the home location and regional format. A
/// display language we ship wins outright — the user picked it. Otherwise
/// the post-Soviet countries outside the Baltics get Russian, recognized by
/// country or by the interface language, and everybody else English.
pub fn pick_language(interface: &str, regions: &[&str]) -> &'static str {
    let interface = interface.trim().replace('_', "-");
    let mut parts = interface.split('-');
    let primary = parts.next().unwrap_or_default().to_ascii_lowercase();
    if primary != FALLBACK {
        if let Some(tag) = resolve(&interface) {
            return tag;
        }
    }
    let interface_region = parts.find(|part| part.len() == 2);
    let russian_region = regions
        .iter()
        .copied()
        .chain(interface_region)
        .any(|region| {
            RUSSIAN_REGIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(region.trim()))
        });
    if russian_region || RUSSIAN_LANGUAGES.contains(&primary.as_str()) {
        if let Some(tag) = resolve("ru") {
            return tag;
        }
    }
    FALLBACK
}
