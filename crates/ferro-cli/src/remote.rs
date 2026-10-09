//! Minimal HTTP client for a Ferro environment.

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

pub struct Remote {
    pub name: String,
    pub url: String,
    pub protected: bool,
    token: String,
    http: reqwest::Client,
}

fn error_message(status: reqwest::StatusCode, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let messages: Vec<String> = parsed
        .as_ref()
        .and_then(|v| v.get("errors"))
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|e| e.get("message").and_then(|m| m.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    if messages.is_empty() {
        format!("HTTP {status}: {}", body.chars().take(500).collect::<String>())
    } else {
        format!("HTTP {status}: {}", messages.join("; "))
    }
}

impl Remote {
    pub fn new(name: &str, url: &str, token: &str, protected: bool) -> anyhow::Result<Remote> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("ferro-cli/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Remote {
            name: name.to_string(),
            url: url.trim_end_matches('/').to_string(),
            protected,
            token: token.to_string(),
            http,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.url, path)
    }

    async fn check(&self, resp: reqwest::Response, what: &str) -> anyhow::Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        bail!("[{}] {what}: {}", self.name, error_message(status, &body))
    }

    /// GET returning the `data` member.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let resp = self
            .http
            .get(self.endpoint(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("[{}] GET {path}", self.name))?;
        let v: Value = self.check(resp, &format!("GET {path}")).await?.json().await?;
        data(v)
    }

    /// POST JSON, returning the `data` member.
    pub async fn post<B: Serialize + ?Sized, T: DeserializeOwned>(&self, path: &str, body: &B) -> anyhow::Result<T> {
        let resp = self
            .http
            .post(self.endpoint(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("[{}] POST {path}", self.name))?;
        let v: Value = self.check(resp, &format!("POST {path}")).await?.json().await?;
        data(v)
    }

    /// POST JSON, returning the raw JSON body (no `data` envelope).
    pub async fn post_raw<B: Serialize + ?Sized, T: DeserializeOwned>(&self, path: &str, body: &B) -> anyhow::Result<T> {
        let resp = self
            .http
            .post(self.endpoint(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("[{}] POST {path}", self.name))?;
        Ok(self.check(resp, &format!("POST {path}")).await?.json().await?)
    }

    pub async fn get_bytes(&self, path: &str) -> anyhow::Result<Bytes> {
        let resp = self
            .http
            .get(self.endpoint(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("[{}] GET {path}", self.name))?;
        Ok(self.check(resp, &format!("GET {path}")).await?.bytes().await?)
    }

    pub async fn put_bytes(&self, path: &str, body: Bytes) -> anyhow::Result<()> {
        let resp = self
            .http
            .put(self.endpoint(path))
            .bearer_auth(&self.token)
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .with_context(|| format!("[{}] PUT {path}", self.name))?;
        self.check(resp, &format!("PUT {path}")).await?;
        Ok(())
    }
}

fn data<T: DeserializeOwned>(mut v: Value) -> anyhow::Result<T> {
    let d = v.get_mut("data").map(Value::take).ok_or_else(|| anyhow!("response without `data`"))?;
    Ok(serde_json::from_value(d)?)
}
