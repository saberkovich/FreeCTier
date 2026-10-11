//! Embeds every `locales/*.json` at the repository root.
//!
//! The directory is the one Crowdin writes to, so a language added there is
//! picked up by the next build without touching code — the window discovers
//! the same files through `import.meta.glob`.

use std::{env, fs, path::PathBuf};

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .join("..")
        .join("..")
        .join("locales");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    let mut code = String::from("pub(crate) static LOCALES: &[(&str, &str)] = &[\n");
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let tag = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("locale file name is UTF-8");
        code.push_str(&format!(
            "    ({tag:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    code.push_str("];\n");
    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir")).join("locales.rs");
    fs::write(out, code).expect("write locales.rs");
}
