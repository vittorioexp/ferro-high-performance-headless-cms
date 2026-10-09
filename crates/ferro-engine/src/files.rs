//! File storage abstraction + upload service.
//!
//! Blobs are stored under the key `<file id>` so that a file keeps the same key in
//! every environment (sync copies blobs by id, no renaming, no folder lookups).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use ferro_core::bundle::Item;
use ferro_core::hash::sha256_hex;
use ferro_core::system::FILES;
use ferro_core::Action;
use serde_json::{Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::items::{raw_get, raw_upsert, Ctx};
use crate::permissions;
use crate::{activity, FerroError, Result};

#[async_trait]
pub trait Storage: Send + Sync {
    /// Name of the location, stored in `ferro_files.storage` (`local`, `s3`, ...).
    fn name(&self) -> &str;
    async fn put(&self, key: &str, data: Bytes, content_type: Option<&str>) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Bytes>;
    async fn exists(&self, key: &str) -> Result<bool>;
    /// Idempotent: deleting a missing blob is not an error.
    async fn delete(&self, key: &str) -> Result<()>;
    /// Startup probe: is the location reachable and writable with the configured credentials?
    async fn check(&self) -> Result<()> {
        Ok(())
    }
    /// A short-lived direct URL for downloading `key`. When a driver returns one the server redirects
    /// to it instead of proxying the bytes. The default (`None`) means "serve it yourself".
    async fn presigned_get(&self, _key: &str, _expires: Duration, _content_disposition: &str, _content_type: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// A blob that belongs to a file row: where it lives and under which key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub storage: String,
    pub key: String,
}

/// The configured storage locations. Each file remembers its location (`ferro_files.storage`);
/// new uploads go to the default one.
#[derive(Clone)]
pub struct Storages {
    default: String,
    map: Arc<BTreeMap<String, Arc<dyn Storage>>>,
}

impl Storages {
    pub fn new(default: &str, locations: Vec<Arc<dyn Storage>>) -> Result<Storages> {
        let mut map = BTreeMap::new();
        for s in locations {
            if map.insert(s.name().to_string(), s).is_some() {
                return Err(FerroError::invalid("two storage locations have the same name"));
            }
        }
        if !map.contains_key(default) {
            return Err(FerroError::invalid(format!("the default storage `{default}` is not configured")));
        }
        Ok(Storages { default: default.to_string(), map: Arc::new(map) })
    }

    pub fn single(storage: Arc<dyn Storage>) -> Storages {
        let name = storage.name().to_string();
        Storages { default: name.clone(), map: Arc::new(BTreeMap::from([(name, storage)])) }
    }

    pub fn default_storage(&self) -> &Arc<dyn Storage> {
        &self.map[&self.default]
    }

    pub fn default_name(&self) -> &str {
        &self.default
    }

    pub fn names(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Storage>> {
        self.map.get(name)
    }

    /// The location named by a file row; rows that point to a location which is no longer configured
    /// are looked up in the default one.
    pub fn resolve(&self, name: Option<&str>) -> &Arc<dyn Storage> {
        match name.and_then(|n| self.map.get(n)) {
            Some(s) => s,
            None => {
                if let Some(n) = name {
                    tracing::warn!(storage = n, "file stored in a location that is not configured: using the default one");
                }
                self.default_storage()
            }
        }
    }

    /// Delete blobs, best effort (called after the database transaction committed).
    pub async fn purge(&self, refs: &[BlobRef]) {
        for r in refs {
            if let Err(e) = self.resolve(Some(&r.storage)).delete(&r.key).await {
                tracing::warn!(error = %e, storage = %r.storage, key = %r.key, "cannot delete a blob");
            }
        }
    }
}

/// The location of an existing file (falls back to the default one).
pub async fn storage_of(conn: &mut PgConnection, storages: &Storages, id: Uuid) -> Result<Arc<dyn Storage>> {
    let name: Option<String> = sqlx::query_scalar::<sqlx::Postgres, String>("SELECT storage FROM ferro_files WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(storages.resolve(name.as_deref()).clone())
}

fn check_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key.len() <= 128
        && !key.contains("..")
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(FerroError::invalid(format!("invalid storage key `{key}`")))
    }
}

/// Local filesystem storage. Use a shared volume when running more than one instance,
/// or an object-store driver (see the `ferro-storage` crate).
pub struct LocalStorage {
    name: String,
    root: PathBuf,
}

impl LocalStorage {
    pub async fn new(root: impl Into<PathBuf>) -> Result<Self> {
        Self::named("local", root).await
    }
    pub async fn named(name: &str, root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        tokio::fs::create_dir_all(&root).await?;
        Ok(LocalStorage { name: name.to_string(), root })
    }
    fn path(&self, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        Ok(self.root.join(key))
    }
}

#[async_trait]
impl Storage for LocalStorage {
    fn name(&self) -> &str {
        &self.name
    }
    async fn put(&self, key: &str, data: Bytes, _content_type: Option<&str>) -> Result<()> {
        let path = self.path(key)?;
        let tmp = self.root.join(format!(".{key}.{}.tmp", Uuid::new_v4()));
        tokio::fs::write(&tmp, &data).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Bytes> {
        let path = self.path(key)?;
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Bytes::from(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FerroError::not_found(format!("blob `{key}` not found"))),
            Err(e) => Err(e.into()),
        }
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        let path = self.path(key)?;
        Ok(tokio::fs::try_exists(&path).await?)
    }
    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    async fn check(&self) -> Result<()> {
        let probe = self.root.join(format!(".probe-{}", Uuid::new_v4()));
        tokio::fs::write(&probe, b"ok").await?;
        tokio::fs::remove_file(&probe).await?;
        Ok(())
    }
}

pub struct Upload {
    pub filename: String,
    pub content_type: Option<String>,
    pub data: Bytes,
    pub folder: Option<Uuid>,
    pub title: Option<String>,
    /// Replace the content of an existing file (keeps id and references).
    pub replace: Option<Uuid>,
}

/// Result of [`upload`]: the file id, and the blob to delete once the transaction has committed when a
/// replaced file moved to another location.
pub struct Uploaded {
    pub id: Uuid,
    pub stale: Option<BlobRef>,
}

/// Store a blob and create/replace its `ferro_files` row.
pub async fn upload(ctx: &Ctx, conn: &mut PgConnection, storage: &dyn Storage, up: Upload) -> Result<Uploaded> {
    let info = ctx.schema.collection(FILES)?;
    let action = if up.replace.is_some() { Action::Update } else { Action::Create };
    let perm = permissions::access(&ctx.acc, FILES, action)?;
    if let Some(r) = up.replace {
        crate::items::ensure_permitted(ctx, &mut *conn, &info, &perm, &[r]).await?;
    }
    let id = up.replace.unwrap_or_else(Uuid::now_v7);
    let existing = match up.replace {
        Some(r) => Some(raw_get(&mut *conn, &info, r).await?.ok_or_else(|| FerroError::not_found("file not found"))?),
        None => None,
    };
    let checksum = sha256_hex(&up.data);
    let size = up.data.len() as i64;
    let content_type = up
        .content_type
        .filter(|c| !c.is_empty() && c != "application/octet-stream")
        .unwrap_or_else(|| mime_from_name(&up.filename));
    storage.put(&id.to_string(), up.data, Some(&content_type)).await?;
    let stale = existing
        .as_ref()
        .and_then(|e| e.get("storage").and_then(|v| v.as_str()))
        .filter(|old| *old != storage.name())
        .map(|old| BlobRef { storage: old.to_string(), key: id.to_string() });
    let now = Utc::now().to_rfc3339();
    let mut row: Item = Map::new();
    row.insert("id".into(), Value::String(id.to_string()));
    row.insert("storage".into(), Value::String(storage.name().to_string()));
    row.insert("filename_disk".into(), Value::String(id.to_string()));
    row.insert("filename_download".into(), Value::String(up.filename.clone()));
    row.insert("type".into(), Value::String(content_type));
    row.insert("filesize".into(), Value::from(size));
    row.insert("checksum".into(), Value::String(checksum));
    if existing.is_none() {
        let title = up.title.unwrap_or_else(|| title_from_name(&up.filename));
        row.insert("title".into(), Value::String(title));
        row.insert("uploaded_on".into(), Value::String(now));
        row.insert("uploaded_by".into(), ctx.acc.user.map(|u| Value::String(u.to_string())).unwrap_or(Value::Null));
        if let Some(f) = up.folder {
            row.insert("folder".into(), Value::String(f.to_string()));
        }
    } else {
        row.insert("modified_on".into(), Value::String(now));
    }
    raw_upsert(&mut *conn, &info, &row).await?;
    let action = if existing.is_some() { "update" } else { "create" };
    activity::record(conn, &ctx.acc, action, FILES, &id.to_string(), ctx.origin, None, None).await?;
    Ok(Uploaded { id, stale })
}

/// Delete file rows (with permissions). The blobs are returned: delete them with
/// [`Storages::purge`] **after** the transaction committed, so a rollback never loses a file.
pub async fn delete_files(ctx: &Ctx, conn: &mut PgConnection, ids: Vec<Uuid>) -> Result<Vec<BlobRef>> {
    let info = ctx.schema.collection(FILES)?;
    let rows = crate::items::raw_by_ids(&mut *conn, &info, &ids).await?;
    crate::items::delete_many(ctx, &mut *conn, FILES, ids.clone()).await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_str()?.to_string();
            let storage = r.get("storage").and_then(|v| v.as_str()).unwrap_or("local").to_string();
            Some(BlobRef { storage, key: id })
        })
        .collect())
}

fn title_from_name(name: &str) -> String {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem.replace(['_', '-'], " ").trim().to_string()
}

fn mime_from_name(name: &str) -> String {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_names() {
        assert!(check_key("0199a000-0000-7000-8000-000000000001").is_ok());
        assert!(check_key("../etc/passwd").is_err());
        assert!(check_key("a/b").is_err());
        assert_eq!(title_from_name("hero_image-final.png"), "hero image final");
        assert_eq!(mime_from_name("X.PDF"), "application/pdf");
    }
}
