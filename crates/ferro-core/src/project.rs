//! Snapshot <-> files. Two formats:
//!
//! * a single file `snapshot.yaml` / `snapshot.json`
//! * a directory (recommended, one file per object = readable git diffs):
//!   ```text
//!   schema/
//!     collections/<collection>.yaml
//!     roles/<role-name>.yaml
//!     policies/<policy-name>.yaml
//!     flows/<flow-name>.yaml
//!   ```

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::model::*;

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ProjectError + '_ {
    move |source| ProjectError::Io { path: path.to_path_buf(), source }
}

fn parse_file<T: DeserializeOwned>(path: &Path) -> Result<T, ProjectError> {
    let text = fs::read_to_string(path).map_err(io(path))?;
    let is_json = path.extension().map(|e| e == "json").unwrap_or(false);
    let res = if is_json {
        serde_json::from_str(&text).map_err(|e| e.to_string())
    } else {
        serde_yaml::from_str(&text).map_err(|e| e.to_string())
    };
    res.map_err(|message| ProjectError::Parse { path: path.to_path_buf(), message })
}

fn yaml_files(dir: &Path) -> Result<Vec<PathBuf>, ProjectError> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for entry in fs::read_dir(dir).map_err(io(dir))? {
        let p = entry.map_err(io(dir))?.path();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if p.is_file() && matches!(ext, "yaml" | "yml" | "json") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

/// Load a snapshot from a file or a schema directory.
pub fn read_snapshot(path: &Path) -> Result<Snapshot, ProjectError> {
    let mut snap = if path.is_file() {
        parse_file::<Snapshot>(path)?
    } else {
        let mut s = Snapshot::default();
        for f in yaml_files(&path.join("collections"))? {
            s.collections.push(parse_file(&f)?);
        }
        for f in yaml_files(&path.join("roles"))? {
            s.roles.push(parse_file(&f)?);
        }
        for f in yaml_files(&path.join("policies"))? {
            s.policies.push(parse_file(&f)?);
        }
        for f in yaml_files(&path.join("flows"))? {
            s.flows.push(parse_file(&f)?);
        }
        if s.collections.is_empty() && s.roles.is_empty() && s.policies.is_empty() && !path.exists() {
            return Err(ProjectError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "schema directory not found"),
            });
        }
        s
    };
    snap.normalize();
    Ok(snap)
}

fn slug(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    out.trim_matches('-').to_string()
}

fn write_yaml<T: Serialize>(path: &Path, value: &T) -> Result<(), ProjectError> {
    let text = serde_yaml::to_string(value)
        .map_err(|e| ProjectError::Parse { path: path.to_path_buf(), message: e.to_string() })?;
    fs::write(path, text).map_err(io(path))
}

/// Write a snapshot. A path ending in `.yaml`/`.yml`/`.json` is written as a single
/// file, anything else as a schema directory (stale files are removed).
pub fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<(), ProjectError> {
    let mut snap = snapshot.clone();
    snap.strip_hints();
    snap.normalize();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "json" => {
            let text = serde_json::to_string_pretty(&snap)
                .map_err(|e| ProjectError::Parse { path: path.to_path_buf(), message: e.to_string() })?;
            return fs::write(path, text).map_err(io(path));
        }
        "yaml" | "yml" => return write_yaml(path, &snap),
        _ => {}
    }
    for (sub, names) in [
        ("collections", snap.collections.iter().map(|c| c.name.clone()).collect::<Vec<_>>()),
        ("roles", snap.roles.iter().map(|r| file_name_for(&r.name, r.id)).collect()),
        ("policies", snap.policies.iter().map(|p| file_name_for(&p.name, p.id)).collect()),
        ("flows", snap.flows.iter().map(|f| flow_file_name(&f.name, f.id)).collect()),
    ] {
        let dir = path.join(sub);
        fs::create_dir_all(&dir).map_err(io(&dir))?;
        for stale in yaml_files(&dir)? {
            let stem = stale.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if !names.contains(&stem) {
                fs::remove_file(&stale).map_err(io(&stale))?;
            }
        }
    }
    for c in &snap.collections {
        write_yaml(&path.join("collections").join(format!("{}.yaml", c.name)), c)?;
    }
    for r in &snap.roles {
        write_yaml(&path.join("roles").join(format!("{}.yaml", file_name_for(&r.name, r.id))), r)?;
    }
    for p in &snap.policies {
        write_yaml(&path.join("policies").join(format!("{}.yaml", file_name_for(&p.name, p.id))), p)?;
    }
    for fl in &snap.flows {
        write_yaml(&path.join("flows").join(format!("{}.yaml", flow_file_name(&fl.name, fl.id))), fl)?;
    }
    Ok(())
}

/// Flows keep a short id suffix: two flows may legitimately share a name.
fn flow_file_name(name: &str, id: uuid::Uuid) -> String {
    let s = slug(name);
    let short: String = id.to_string().chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    if s.is_empty() {
        id.to_string()
    } else {
        format!("{s}-{short}")
    }
}

fn file_name_for(name: &str, id: uuid::Uuid) -> String {
    let s = slug(name);
    if s.is_empty() {
        id.to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_roundtrip() {
        let snap: Snapshot = serde_yaml::from_str(include_str!("../tests/fixtures/pages.yaml")).unwrap();
        let dir = std::env::temp_dir().join(format!("ferro-test-{}", uuid::Uuid::new_v4()));
        write_snapshot(&dir, &snap).unwrap();
        assert!(dir.join("collections/pages.yaml").exists());
        assert!(dir.join("policies/editor.yaml").exists());
        let back = read_snapshot(&dir).unwrap();
        assert_eq!(back.hash(), snap.hash());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn example_project_is_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/site/schema");
        let snap = read_snapshot(&dir).unwrap();
        assert_eq!(snap.collections.len(), 10);
        assert_eq!(snap.policies.len(), 2);
        assert_eq!(snap.flows.len(), 5);
        let errors = crate::validate::validate_snapshot(&snap);
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
