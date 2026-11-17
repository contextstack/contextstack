//! Repository inventories reported by machines, stored in the Director's node records.

use crate::{records, server::State, store::Record};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

/// `GET repos`: each row identifies its machine as well as its local checkout.
pub fn list(state: &State) -> Result<Value, String> {
    let nodes = records::list(&state.store.db(), Record::Node, None).map_err(|e| e.to_string())?;
    let mut repos = Vec::new();
    for node in nodes {
        if !node["revoked_at"].is_null() {
            continue;
        }
        if let Some(rows) = node["status"]["repos"].as_array() {
            for row in rows {
                let mut repo = row.clone();
                repo["node"] = node["name"].clone();
                repos.push(repo);
            }
        }
    }
    Ok(json!({"repos": repos}))
}

/// Checkout names are reserved because lane worktrees live beside the main checkout.
pub fn lane_name(repo: &str, requested: &str, entry: &Value) -> String {
    let mut reserved = vec![repo.to_owned(), "main".into(), "master".into()];
    if let Some(branch) = entry["branch"].as_str() {
        reserved.push(branch.into());
    }
    if let Some(name) = entry["path"]
        .as_str()
        .or(entry.as_str())
        .and_then(|p| std::path::Path::new(p).file_name())
        .and_then(|n| n.to_str())
    {
        reserved.push(name.into());
    }
    let mut chosen = requested.to_owned();
    let mut suffix = 1;
    while reserved.contains(&chosen) {
        let base = &requested[..requested.len().min(58)];
        chosen = format!("{base}-{suffix}");
        suffix += 1;
    }
    chosen
}

pub fn entry(repo: &str) -> Result<Value, String> {
    let path = repos_path();
    if !path.is_file() {
        return Ok(Value::Null);
    }
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let entries: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    Ok(entries[repo].clone())
}

fn repos_path() -> PathBuf {
    std::env::var("CS_REPOS_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map_or_else(
            || {
                PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join(".contextstack/config/repos.json")
            },
            PathBuf::from,
        )
}
