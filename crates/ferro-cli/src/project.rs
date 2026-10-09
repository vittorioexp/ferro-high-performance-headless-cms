//! `ferro.toml`: the project file versioned together with the schema.
//!
//! ```toml
//! [project]
//! schema = "schema"            # schema directory (collections/, roles/, policies/)
//!
//! [environments.dev]
//! url = "https://cms-dev.example.com"
//! token_env = "FERRO_DEV_TOKEN" # the token itself is never written in the file
//!
//! [environments.prod]
//! url = "https://cms.example.com"
//! token_env = "FERRO_PROD_TOKEN"
//! protected = true             # writes require --yes or typing the environment name
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::remote::Remote;

#[derive(Debug, Default, Deserialize)]
pub struct ProjectFile {
    #[serde(default)]
    pub project: ProjectSection,
    #[serde(default)]
    pub environments: BTreeMap<String, EnvSection>,
}

#[derive(Debug, Deserialize)]
pub struct ProjectSection {
    #[serde(default = "default_schema_dir")]
    pub schema: PathBuf,
}

impl Default for ProjectSection {
    fn default() -> Self {
        ProjectSection { schema: default_schema_dir() }
    }
}

fn default_schema_dir() -> PathBuf {
    PathBuf::from("schema")
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvSection {
    pub url: String,
    #[serde(default)]
    pub token_env: Option<String>,
    /// Discouraged: prefer `token_env`.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub protected: bool,
}

pub struct Project {
    pub root: PathBuf,
    pub file: ProjectFile,
}

impl Project {
    pub fn load(path: &Path) -> anyhow::Result<Project> {
        let root = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        if !path.exists() {
            return Ok(Project { root, file: ProjectFile::default() });
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: ProjectFile = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Project { root, file })
    }

    pub fn schema_dir(&self) -> PathBuf {
        self.root.join(&self.file.project.schema)
    }

    pub fn has_env(&self, name: &str) -> bool {
        self.file.environments.contains_key(name.trim_start_matches('@'))
    }

    pub fn env(&self, name: &str) -> anyhow::Result<Remote> {
        let key = name.trim_start_matches('@');
        let Some(e) = self.file.environments.get(key) else {
            let known: Vec<&String> = self.file.environments.keys().collect();
            bail!("unknown environment `{key}` (known: {known:?}); define it in ferro.toml");
        };
        let token = match (&e.token_env, &e.token) {
            (Some(var), _) => std::env::var(var).with_context(|| format!("environment `{key}`: variable {var} is not set"))?,
            (None, Some(t)) => t.clone(),
            (None, None) => std::env::var(format!("FERRO_{}_TOKEN", key.to_uppercase()))
                .with_context(|| format!("environment `{key}`: set token_env in ferro.toml or FERRO_{}_TOKEN", key.to_uppercase()))?,
        };
        Remote::new(key, &e.url, &token, e.protected)
    }
}

pub const TEMPLATE: &str = r#"# Ferro project file
[project]
schema = "schema"

# One section per environment. Tokens are read from environment variables.
[environments.local]
url = "http://localhost:8055"
token_env = "FERRO_LOCAL_TOKEN"

[environments.dev]
url = "https://cms-dev.example.com"
token_env = "FERRO_DEV_TOKEN"

[environments.test]
url = "https://cms-test.example.com"
token_env = "FERRO_TEST_TOKEN"

[environments.prod]
url = "https://cms.example.com"
token_env = "FERRO_PROD_TOKEN"
protected = true
"#;
