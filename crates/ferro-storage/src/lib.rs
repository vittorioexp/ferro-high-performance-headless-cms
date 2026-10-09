//! Storage locations of Ferro.
//!
//! A *location* has a name (stored with every file, `ferro_files.storage`) and a driver:
//!
//! | driver | for |
//! |---|---|
//! | `local` | a directory (a shared volume when several instances run) |
//! | `s3` | Amazon S3 and every S3-compatible service: MinIO, Cloudflare R2, Backblaze B2, Wasabi, DigitalOcean Spaces, Google Cloud Storage (HMAC keys) |
//! | `azure` | Azure Blob Storage, authenticated with a SAS token |
//!
//! Configuration is read from environment variables, see [`config`].

mod azure;
pub mod config;
mod s3;

use std::sync::Arc;

use ferro_engine::files::{LocalStorage, Storage, Storages};
use ferro_engine::{FerroError, Result};

pub use azure::AzureStorage;
pub use config::{Driver, Location, StorageConfig};
pub use s3::S3Storage;

/// Create the drivers, probe each location (a misconfiguration must stop the server at startup, not at the
/// first upload) and assemble the registry.
pub async fn build(cfg: &StorageConfig) -> Result<Storages> {
    let mut list: Vec<Arc<dyn Storage>> = Vec::new();
    for loc in &cfg.locations {
        let storage: Arc<dyn Storage> = match &loc.driver {
            Driver::Local { root } => Arc::new(LocalStorage::named(&loc.name, root).await?),
            Driver::S3(o) => Arc::new(S3Storage::connect(&loc.name, o).await?),
            Driver::Azure(o) => Arc::new(AzureStorage::new(&loc.name, o)?),
        };
        storage
            .check()
            .await
            .map_err(|e| FerroError::Internal(format!("storage location `{}` is not usable: {e}", loc.name)))?;
        tracing::info!(location = %loc.name, driver = loc.driver.label(), default = loc.name == cfg.default, "storage ready");
        list.push(storage);
    }
    Storages::new(&cfg.default, list)
}
