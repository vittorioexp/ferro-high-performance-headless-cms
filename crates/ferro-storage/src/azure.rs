//! Azure Blob Storage through the REST API, authenticated with a SAS token (no SDK needed).

use async_trait::async_trait;
use bytes::Bytes;
use ferro_engine::files::Storage;
use ferro_engine::{FerroError, Result};
use reqwest::{Client, StatusCode};

use crate::config::AzureOptions;

pub struct AzureStorage {
    name: String,
    http: Client,
    base: String,
    sas: String,
    prefix: String,
}

impl AzureStorage {
    pub fn new(name: &str, o: &AzureOptions) -> Result<AzureStorage> {
        Ok(AzureStorage {
            name: name.to_string(),
            http: Client::builder().timeout(std::time::Duration::from_secs(300)).build().map_err(|e| FerroError::Internal(e.to_string()))?,
            base: format!("{}/{}", o.account_url, o.container),
            sas: o.sas_token.clone(),
            prefix: o.prefix.clone(),
        })
    }

    fn url(&self, key: &str) -> String {
        format!("{}/{}{key}?{}", self.base, self.prefix, self.sas)
    }
}

fn io<E: std::fmt::Display>(what: &str, e: E) -> FerroError {
    FerroError::Internal(format!("azure {what}: {e}"))
}

async fn status_error(what: &str, resp: reqwest::Response) -> FerroError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    FerroError::Internal(format!("azure {what}: HTTP {status} {}", body.chars().take(300).collect::<String>()))
}

#[async_trait]
impl Storage for AzureStorage {
    fn name(&self) -> &str {
        &self.name
    }

    async fn put(&self, key: &str, data: Bytes, content_type: Option<&str>) -> Result<()> {
        let resp = self
            .http
            .put(self.url(key))
            .header("x-ms-blob-type", "BlockBlob")
            .header("x-ms-version", "2021-08-06")
            .header("x-ms-blob-content-type", content_type.unwrap_or("application/octet-stream"))
            .body(data)
            .send()
            .await
            .map_err(|e| io("put", e))?;
        if resp.status().is_success() { Ok(()) } else { Err(status_error("put", resp).await) }
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let resp = self.http.get(self.url(key)).header("x-ms-version", "2021-08-06").send().await.map_err(|e| io("get", e))?;
        match resp.status() {
            s if s.is_success() => resp.bytes().await.map_err(|e| io("get", e)),
            StatusCode::NOT_FOUND => Err(FerroError::not_found(format!("blob `{key}` not found"))),
            _ => Err(status_error("get", resp).await),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let resp = self.http.head(self.url(key)).header("x-ms-version", "2021-08-06").send().await.map_err(|e| io("head", e))?;
        match resp.status() {
            s if s.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(status_error("head", resp).await),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let resp = self.http.delete(self.url(key)).header("x-ms-version", "2021-08-06").send().await.map_err(|e| io("delete", e))?;
        match resp.status() {
            s if s.is_success() => Ok(()),
            StatusCode::NOT_FOUND => Ok(()),
            _ => Err(status_error("delete", resp).await),
        }
    }

    async fn check(&self) -> Result<()> {
        let probe = format!(".ferro-probe-{}", std::process::id());
        self.put(&probe, Bytes::from_static(b"ok"), Some("text/plain")).await?;
        self.delete(&probe).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    type Seen = Arc<Mutex<Vec<(String, String, HashMap<String, String>)>>>;

    /// A minimal in-memory imitation of the Blob REST API (just what the driver uses).
    async fn fake_blob_service() -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Seen = Arc::default();
        let blobs: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let (blobs, log) = (blobs.clone(), log.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let (head_end, len) = loop {
                        let mut chunk = [0u8; 4096];
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                            let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).map(|v| v.trim().parse::<usize>().unwrap()).unwrap_or(0);
                            break (p + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let mut chunk = [0u8; 4096];
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut lines = head.lines();
                    let mut first = lines.next().unwrap().split(' ');
                    let (method, target) = (first.next().unwrap().to_string(), first.next().unwrap().to_string());
                    let headers: HashMap<String, String> =
                        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
                    let body = buf[head_end..head_end + len].to_vec();
                    log.lock().unwrap().push((method.clone(), target.clone(), headers));
                    let path = target.split('?').next().unwrap().to_string();
                    let (status, payload): (&str, Vec<u8>) = match method.as_str() {
                        "PUT" => {
                            blobs.lock().unwrap().insert(path, body);
                            ("201 Created", vec![])
                        }
                        "GET" => match blobs.lock().unwrap().get(&path) {
                            Some(b) => ("200 OK", b.clone()),
                            None => ("404 Not Found", b"<Error><Code>BlobNotFound</Code></Error>".to_vec()),
                        },
                        "HEAD" => (if blobs.lock().unwrap().contains_key(&path) { "200 OK" } else { "404 Not Found" }, vec![]),
                        "DELETE" => match blobs.lock().unwrap().remove(&path) {
                            Some(_) => ("202 Accepted", vec![]),
                            None => ("404 Not Found", vec![]),
                        },
                        _ => ("405 Method Not Allowed", vec![]),
                    };
                    let resp = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", if method == "HEAD" { 0 } else { payload.len() });
                    sock.write_all(resp.as_bytes()).await.unwrap();
                    if method != "HEAD" {
                        sock.write_all(&payload).await.unwrap();
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn blob_lifecycle_against_a_fake_service() {
        let (url, seen) = fake_blob_service().await;
        let s = AzureStorage::new("blobs", &AzureOptions { account_url: url, container: "ferro".into(), sas_token: "sv=1&sig=abc".into(), prefix: "p/".into() }).unwrap();
        s.check().await.expect("write + delete probe");
        assert!(!s.exists("k1").await.unwrap());
        s.put("k1", Bytes::from_static(b"payload"), Some("text/plain")).await.unwrap();
        assert!(s.exists("k1").await.unwrap());
        assert_eq!(&s.get("k1").await.unwrap()[..], b"payload");
        s.delete("k1").await.unwrap();
        s.delete("k1").await.unwrap(); // idempotent: a missing blob is not an error
        assert!(matches!(s.get("k1").await, Err(FerroError::NotFound(_))));
        let log = seen.lock().unwrap();
        let put = log.iter().find(|(m, t, _)| m == "PUT" && t.contains("/ferro/p/k1")).expect("PUT of k1");
        assert!(put.1.ends_with("?sv=1&sig=abc"), "SAS token appended to the URL: {}", put.1);
        assert_eq!(put.2.get("x-ms-blob-type").map(String::as_str), Some("BlockBlob"));
        assert_eq!(put.2.get("x-ms-blob-content-type").map(String::as_str), Some("text/plain"));
    }

    #[tokio::test]
    async fn other_failures_are_errors() {
        // nothing listens here: connection refused must be reported, not mistaken for "not found"
        let s = AzureStorage::new("blobs", &AzureOptions { account_url: "http://127.0.0.1:1".into(), container: "c".into(), sas_token: "x".into(), prefix: String::new() }).unwrap();
        assert!(matches!(s.get("k").await, Err(FerroError::Internal(_))));
        assert!(s.exists("k").await.is_err());
        assert!(s.check().await.is_err());
    }
}
