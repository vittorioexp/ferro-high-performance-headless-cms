//! Amazon S3 and S3-compatible services (MinIO, R2, B2, Wasabi, Spaces, GCS with HMAC keys).

use std::time::Duration;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::Bytes;
use ferro_engine::files::Storage;
use ferro_engine::{FerroError, Result};

use crate::config::S3Options;

pub struct S3Storage {
    name: String,
    client: Client,
    bucket: String,
    prefix: String,
    presign: Option<Duration>,
}

fn internal<E: std::error::Error + 'static>(what: &str, e: aws_sdk_s3::error::SdkError<E>) -> FerroError {
    // `Display` of SdkError is just "service error"; DisplayErrorContext walks the cause chain to the S3 code.
    FerroError::Internal(format!("s3 {what}: {}", aws_sdk_s3::error::DisplayErrorContext(&e)))
}

impl S3Storage {
    pub async fn connect(name: &str, o: &S3Options) -> Result<S3Storage> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(o.region.clone()));
        if let Some((key, secret, token)) = &o.credentials {
            loader = loader.credentials_provider(Credentials::new(key, secret, token.clone(), None, "ferro-config"));
        }
        let shared = loader.load().await;
        let mut b = aws_sdk_s3::config::Builder::from(&shared).force_path_style(o.force_path_style);
        if let Some(ep) = &o.endpoint {
            b = b.endpoint_url(ep);
        }
        Ok(S3Storage {
            name: name.to_string(),
            client: Client::from_conf(b.build()),
            bucket: o.bucket.clone(),
            prefix: o.prefix.clone(),
            presign: o.presign_secs.map(Duration::from_secs),
        })
    }

    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }
}

#[async_trait]
impl Storage for S3Storage {
    fn name(&self) -> &str {
        &self.name
    }

    async fn put(&self, key: &str, data: Bytes, content_type: Option<&str>) -> Result<()> {
        let mut req = self.client.put_object().bucket(&self.bucket).key(self.key(key)).body(ByteStream::from(data));
        if let Some(ct) = content_type {
            req = req.content_type(ct);
        }
        req.send().await.map_err(|e| internal("put", e))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        match self.client.get_object().bucket(&self.bucket).key(self.key(key)).send().await {
            Ok(out) => {
                let data = out.body.collect().await.map_err(|e| FerroError::Internal(format!("s3 get: {e}")))?;
                Ok(data.into_bytes())
            }
            Err(e) if e.as_service_error().map(|s| s.is_no_such_key()).unwrap_or(false) => {
                Err(FerroError::not_found(format!("blob `{key}` not found")))
            }
            Err(e) => Err(internal("get", e)),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        match self.client.head_object().bucket(&self.bucket).key(self.key(key)).send().await {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().map(|s| s.is_not_found()).unwrap_or(false) => Ok(false),
            Err(e) => Err(internal("head", e)),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        // S3 answers 204 for a missing key, so delete is idempotent by itself.
        self.client.delete_object().bucket(&self.bucket).key(self.key(key)).send().await.map_err(|e| internal("delete", e))?;
        Ok(())
    }

    async fn check(&self) -> Result<()> {
        self.client.head_bucket().bucket(&self.bucket).send().await.map_err(|e| internal(&format!("bucket `{}`", self.bucket), e))?;
        // Reading the bucket is not enough: prove write + delete with a throw-away object.
        let probe = format!(".ferro-probe-{}", std::process::id());
        self.put(&probe, Bytes::from_static(b"ok"), Some("text/plain")).await?;
        self.delete(&probe).await
    }

    async fn presigned_get(&self, key: &str, expires: Duration, content_disposition: &str, content_type: &str) -> Result<Option<String>> {
        let Some(max) = self.presign else { return Ok(None) };
        let expires = expires.min(max);
        let cfg = PresigningConfig::expires_in(expires).map_err(|e| FerroError::Internal(format!("s3 presign: {e}")))?;
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .response_content_disposition(content_disposition)
            .response_content_type(content_type)
            .presigned(cfg)
            .await
            .map_err(|e| internal("presign", e))?;
        Ok(Some(req.uri().to_string()))
    }
}
