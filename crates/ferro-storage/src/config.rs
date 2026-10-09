//! Storage configuration from environment variables.
//!
//! ```text
//! FERRO_STORAGE_LOCATIONS=local,assets        names of the locations (default: local)
//! FERRO_STORAGE_DEFAULT=assets                where new uploads go (default: the first one)
//!
//! FERRO_STORAGE_LOCAL_ROOT=./uploads          driver local (the default driver of a location called "local")
//!
//! FERRO_STORAGE_ASSETS_DRIVER=s3
//! FERRO_STORAGE_ASSETS_BUCKET=my-bucket
//! FERRO_STORAGE_ASSETS_REGION=eu-south-1      (default: AWS_REGION, else us-east-1)
//! FERRO_STORAGE_ASSETS_PREFIX=ferro/prod/     optional key prefix
//! FERRO_STORAGE_ASSETS_ENDPOINT=http://minio:9000   only for S3-compatible services
//! FERRO_STORAGE_ASSETS_FORCE_PATH_STYLE=true  (default true when an endpoint is set)
//! FERRO_STORAGE_ASSETS_ACCESS_KEY_ID / _SECRET_ACCESS_KEY / _SESSION_TOKEN
//!                                             optional: without them the standard AWS credential chain is used
//!                                             (environment, profile, ECS task role, EKS web identity, EC2 instance role)
//! FERRO_STORAGE_ASSETS_PRESIGN=300            downloads redirect to a signed URL valid that many seconds (default: off)
//!
//! FERRO_STORAGE_BLOBS_DRIVER=azure
//! FERRO_STORAGE_BLOBS_ACCOUNT_URL=https://acct.blob.core.windows.net
//! FERRO_STORAGE_BLOBS_CONTAINER=ferro
//! FERRO_STORAGE_BLOBS_SAS_TOKEN=sv=...&sig=...
//! FERRO_STORAGE_BLOBS_PREFIX=optional/
//! ```
//! A location name may contain letters, digits, `-` and `_`; in variable names it is upper-cased with `-` as `_`.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct S3Options {
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub force_path_style: bool,
    pub credentials: Option<(String, String, Option<String>)>,
    /// Seconds a signed download URL stays valid; `None` = the server streams the bytes itself.
    pub presign_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AzureOptions {
    pub account_url: String,
    pub container: String,
    pub sas_token: String,
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Driver {
    Local { root: PathBuf },
    S3(S3Options),
    Azure(AzureOptions),
}

impl Driver {
    pub fn label(&self) -> &'static str {
        match self {
            Driver::Local { .. } => "local",
            Driver::S3(_) => "s3",
            Driver::Azure(_) => "azure",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub name: String,
    pub driver: Driver,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    pub locations: Vec<Location>,
    pub default: String,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !n.starts_with('-')
}

fn var_prefix(name: &str) -> String {
    format!("FERRO_STORAGE_{}_", name.to_ascii_uppercase().replace('-', "_"))
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn normalize_prefix(p: &str) -> String {
    let p = p.trim().trim_start_matches('/');
    if p.is_empty() || p.ends_with('/') {
        p.to_string()
    } else {
        format!("{p}/")
    }
}

impl StorageConfig {
    pub fn from_env() -> Result<StorageConfig, String> {
        Self::from_vars(&|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }

    /// `get` returns the value of a variable (testable without touching the process environment).
    pub fn from_vars(get: &dyn Fn(&str) -> Option<String>) -> Result<StorageConfig, String> {
        let names: Vec<String> = get("FERRO_STORAGE_LOCATIONS")
            .unwrap_or_else(|| "local".to_string())
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if names.is_empty() {
            return Err("FERRO_STORAGE_LOCATIONS is empty".to_string());
        }
        let mut locations: Vec<Location> = Vec::new();
        for name in &names {
            if !valid_name(name) {
                return Err(format!("invalid storage location name `{name}` (letters, digits, - and _)"));
            }
            if locations.iter().any(|l| &l.name == name) {
                return Err(format!("storage location `{name}` is listed twice"));
            }
            let p = var_prefix(name);
            let v = |suffix: &str| get(&format!("{p}{suffix}"));
            let driver_name = v("DRIVER").unwrap_or_else(|| if name == "local" { "local".to_string() } else { String::new() });
            let driver = match driver_name.to_ascii_lowercase().as_str() {
                "local" => Driver::Local { root: PathBuf::from(v("ROOT").unwrap_or_else(|| "./uploads".to_string())) },
                "s3" => {
                    let endpoint = v("ENDPOINT");
                    let creds = match (v("ACCESS_KEY_ID"), v("SECRET_ACCESS_KEY")) {
                        (Some(k), Some(s)) => Some((k, s, v("SESSION_TOKEN"))),
                        (None, None) => None,
                        _ => return Err(format!("{p}ACCESS_KEY_ID and {p}SECRET_ACCESS_KEY must be set together")),
                    };
                    let presign_secs = match v("PRESIGN") {
                        None => None,
                        Some(s) if matches!(s.to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no") => None,
                        Some(s) => Some(s.parse::<u64>().map_err(|_| format!("{p}PRESIGN must be a number of seconds"))?.clamp(1, 7 * 24 * 3600)),
                    };
                    Driver::S3(S3Options {
                        bucket: v("BUCKET").ok_or_else(|| format!("{p}BUCKET is required"))?,
                        region: v("REGION").or_else(|| get("AWS_REGION")).or_else(|| get("AWS_DEFAULT_REGION")).unwrap_or_else(|| "us-east-1".to_string()),
                        prefix: normalize_prefix(&v("PREFIX").unwrap_or_default()),
                        force_path_style: v("FORCE_PATH_STYLE").map(|s| truthy(&s)).unwrap_or(endpoint.is_some()),
                        endpoint,
                        credentials: creds,
                        presign_secs,
                    })
                }
                "azure" => Driver::Azure(AzureOptions {
                    account_url: v("ACCOUNT_URL").ok_or_else(|| format!("{p}ACCOUNT_URL is required"))?.trim_end_matches('/').to_string(),
                    container: v("CONTAINER").ok_or_else(|| format!("{p}CONTAINER is required"))?,
                    sas_token: v("SAS_TOKEN").ok_or_else(|| format!("{p}SAS_TOKEN is required"))?.trim_start_matches('?').to_string(),
                    prefix: normalize_prefix(&v("PREFIX").unwrap_or_default()),
                }),
                "" => return Err(format!("{p}DRIVER is required (local, s3 or azure)")),
                other => return Err(format!("{p}DRIVER: unknown driver `{other}` (local, s3 or azure)")),
            };
            locations.push(Location { name: name.clone(), driver });
        }
        let default = get("FERRO_STORAGE_DEFAULT").map(|s| s.trim().to_ascii_lowercase()).unwrap_or_else(|| names[0].clone());
        if !locations.iter().any(|l| l.name == default) {
            return Err(format!("FERRO_STORAGE_DEFAULT `{default}` is not one of the locations ({})", names.join(", ")));
        }
        Ok(StorageConfig { locations, default })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<StorageConfig, String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        StorageConfig::from_vars(&|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_to_a_local_directory() {
        let c = cfg(&[]).unwrap();
        assert_eq!(c.default, "local");
        assert_eq!(c.locations, vec![Location { name: "local".into(), driver: Driver::Local { root: "./uploads".into() } }]);
        let c = cfg(&[("FERRO_STORAGE_LOCAL_ROOT", "/data/files")]).unwrap();
        assert_eq!(c.locations[0].driver, Driver::Local { root: "/data/files".into() });
    }

    #[test]
    fn s3_with_a_compatible_endpoint() {
        let c = cfg(&[
            ("FERRO_STORAGE_LOCATIONS", "local,Assets"),
            ("FERRO_STORAGE_DEFAULT", "assets"),
            ("FERRO_STORAGE_ASSETS_DRIVER", "s3"),
            ("FERRO_STORAGE_ASSETS_BUCKET", "b"),
            ("FERRO_STORAGE_ASSETS_ENDPOINT", "http://minio:9000"),
            ("FERRO_STORAGE_ASSETS_ACCESS_KEY_ID", "k"),
            ("FERRO_STORAGE_ASSETS_SECRET_ACCESS_KEY", "s"),
            ("FERRO_STORAGE_ASSETS_PREFIX", "/ferro/prod"),
            ("FERRO_STORAGE_ASSETS_PRESIGN", "120"),
        ])
        .unwrap();
        assert_eq!(c.default, "assets");
        let Driver::S3(o) = &c.locations[1].driver else { panic!("s3 expected") };
        assert_eq!(o.bucket, "b");
        assert_eq!(o.region, "us-east-1");
        assert_eq!(o.prefix, "ferro/prod/");
        assert!(o.force_path_style, "path style is the default with a custom endpoint");
        assert_eq!(o.credentials, Some(("k".into(), "s".into(), None)));
        assert_eq!(o.presign_secs, Some(120));
    }

    #[test]
    fn aws_defaults_use_the_credential_chain_and_virtual_hosted_style() {
        let c = cfg(&[("FERRO_STORAGE_LOCATIONS", "s3"), ("FERRO_STORAGE_S3_DRIVER", "s3"), ("FERRO_STORAGE_S3_BUCKET", "b"), ("AWS_REGION", "eu-south-1")]).unwrap();
        let Driver::S3(o) = &c.locations[0].driver else { panic!() };
        assert_eq!((o.region.as_str(), o.credentials.clone(), o.force_path_style, o.presign_secs), ("eu-south-1", None, false, None));
    }

    #[test]
    fn azure_location() {
        let c = cfg(&[
            ("FERRO_STORAGE_LOCATIONS", "blob-store"),
            ("FERRO_STORAGE_BLOB_STORE_DRIVER", "azure"),
            ("FERRO_STORAGE_BLOB_STORE_ACCOUNT_URL", "https://acct.blob.core.windows.net/"),
            ("FERRO_STORAGE_BLOB_STORE_CONTAINER", "ferro"),
            ("FERRO_STORAGE_BLOB_STORE_SAS_TOKEN", "?sv=1&sig=abc"),
        ])
        .unwrap();
        let Driver::Azure(o) = &c.locations[0].driver else { panic!() };
        assert_eq!((o.account_url.as_str(), o.sas_token.as_str()), ("https://acct.blob.core.windows.net", "sv=1&sig=abc"));
    }

    #[test]
    fn reports_misconfigurations() {
        let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
            (vec![("FERRO_STORAGE_LOCATIONS", "x")], "FERRO_STORAGE_X_DRIVER is required"),
            (vec![("FERRO_STORAGE_LOCATIONS", "x"), ("FERRO_STORAGE_X_DRIVER", "ftp")], "unknown driver"),
            (vec![("FERRO_STORAGE_LOCATIONS", "x"), ("FERRO_STORAGE_X_DRIVER", "s3")], "BUCKET is required"),
            (vec![("FERRO_STORAGE_LOCATIONS", "x"), ("FERRO_STORAGE_X_DRIVER", "s3"), ("FERRO_STORAGE_X_BUCKET", "b"), ("FERRO_STORAGE_X_ACCESS_KEY_ID", "k")], "must be set together"),
            (vec![("FERRO_STORAGE_LOCATIONS", "x"), ("FERRO_STORAGE_X_DRIVER", "local"), ("FERRO_STORAGE_DEFAULT", "y")], "not one of the locations"),
            (vec![("FERRO_STORAGE_LOCATIONS", "local,local")], "listed twice"),
            (vec![("FERRO_STORAGE_LOCATIONS", "bad name")], "invalid storage location name"),
            (vec![("FERRO_STORAGE_LOCATIONS", "x"), ("FERRO_STORAGE_X_DRIVER", "s3"), ("FERRO_STORAGE_X_BUCKET", "b"), ("FERRO_STORAGE_X_PRESIGN", "soon")], "PRESIGN must be a number"),
        ];
        for (vars, needle) in cases {
            let err = cfg(&vars).unwrap_err();
            assert!(err.contains(needle), "{err} should contain {needle}");
        }
    }
}
