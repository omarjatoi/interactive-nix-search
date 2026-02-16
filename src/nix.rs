use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

use serde::Deserialize;

const CACHE_MAX_AGE_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct Package {
    /// Leaf name, e.g. "uv"
    pub name: String,
    /// Package set prefix, e.g. "python314Packages" (empty for top-level)
    pub package_set: String,
    pub version: String,
    pub description: String,
}

#[derive(Deserialize)]
struct NixSearchEntry {
    #[serde(default)]
    description: String,
    #[allow(dead_code)]
    #[serde(default)]
    pname: String,
    #[serde(default)]
    version: String,
}

/// Strip the `legacyPackages.<system>.` prefix and split into (package_set, leaf_name).
/// e.g. "legacyPackages.aarch64-darwin.python314Packages.uv" -> ("python314Packages", "uv")
/// e.g. "legacyPackages.aarch64-darwin.ruff" -> ("", "ruff")
fn split_attr_path(attr_path: &str) -> (&str, &str) {
    let remainder = attr_path
        .strip_prefix("legacyPackages.")
        .and_then(|s| s.find('.').map(|i| &s[i + 1..]))
        .unwrap_or(attr_path);

    match remainder.rfind('.') {
        Some(i) => (&remainder[..i], &remainder[i + 1..]),
        None => ("", remainder),
    }
}

fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("interactive-nix-search"))
}

fn cache_path(flake: &str) -> Option<PathBuf> {
    let safe_name: String = flake
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cache_dir().map(|d| d.join(format!("{safe_name}.json")))
}

fn is_cache_fresh(path: &PathBuf) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .and_then(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .map_err(io::Error::other)
        })
        .map(|age| age.as_secs() < CACHE_MAX_AGE_SECS)
        .unwrap_or(false)
}

fn parse_packages(data: &[u8]) -> io::Result<Vec<Package>> {
    let entries: HashMap<String, NixSearchEntry> =
        serde_json::from_slice(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let packages = entries
        .into_iter()
        .map(|(attr_path, entry)| {
            let (package_set, leaf) = split_attr_path(&attr_path);
            Package {
                name: leaf.to_string(),
                package_set: package_set.to_string(),
                version: entry.version,
                description: entry.description,
            }
        })
        .collect();

    Ok(packages)
}

pub fn load_packages(flake: &str) -> io::Result<Vec<Package>> {
    if let Some(path) = cache_path(flake)
        && is_cache_fresh(&path)
    {
        let data = fs::read(&path)?;
        return parse_packages(&data);
    }

    let output = Command::new("nix")
        .args(["search", flake, "--json", "."])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!("nix search failed: {stderr}")));
    }

    if let Some(path) = cache_path(flake) {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, &output.stdout)?;
    }

    parse_packages(&output.stdout)
}
