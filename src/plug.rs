#![allow(dead_code)]

use crate::core::{FsError, Result};
use crate::http::HttpConfig;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub data: Vec<u8>,
    pub status: u16,
    pub content_length: Option<u64>,
    pub content_range: Option<(u64, u64)>,
    /// Parsed Retry-After value in seconds, if server sent one.
    pub retry_after_secs: Option<u64>,
}

impl HttpResponse {
    pub fn new(
        data: Vec<u8>,
        status: u16,
        content_length: Option<u64>,
        content_range: Option<(u64, u64)>,
        retry_after_secs: Option<u64>,
    ) -> Self {
        Self {
            data,
            status,
            content_length,
            content_range,
            retry_after_secs,
        }
    }
}

/// Async transport trait — internal only.
#[async_trait::async_trait]
pub trait AsyncHttp: Send + Sync {
    async fn get_content_length(&self, url: &str) -> Result<Option<u64>>;
    async fn get_range(&self, url: &str, start: u64, end: u64) -> Result<HttpResponse>;
}

pub(crate) fn build_default_transport(config: &HttpConfig) -> Arc<dyn AsyncHttp> {
    #[cfg(feature = "reqwest")]
    {
        Arc::new(ReqwestAsyncTransport::new(config))
    }
    #[cfg(all(not(feature = "reqwest"), feature = "curl"))]
    {
        // curl stays blocking; we run it on a spawn_blocking thread inside the async wrapper.
        Arc::new(CurlAsyncTransport::new(config))
    }
}

pub(crate) fn parse_content_range(header: &str) -> Option<(u64, u64)> {
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 2 || parts[0] != "bytes" {
        return None;
    }
    let range_part = parts[1].split('/').next()?;
    let mut it = range_part.split('-');
    let start = it.next()?.parse::<u64>().ok()?;
    let end = it.next()?.parse::<u64>().ok()?;
    Some((start, end))
}

/// Parse Retry-After either an integer seconds value or an HTTP-date.
pub(crate) fn parse_retry_after(header: &str) -> Option<u64> {
    // Try plain integer first.
    if let Ok(secs) = header.trim().parse::<u64>() {
        return Some(secs);
    }
    // HTTP-date parsing is complex; return a safe default so callers back off.
    Some(5)
}

pub(crate) fn validate_range_response(
    status: u16,
    content_range: Option<(u64, u64)>,
    requested_start: u64,
    retry_after_secs: Option<u64>,
) -> Result<()> {
    if status == 429 || status == 503 {
        return Err(FsError::RateLimited { retry_after_secs });
    }
    if status == 416 {
        // Range not satisfiable  treat as EOF, handled by caller.
        return Ok(());
    }
    if status == 200 {
        return Err(FsError::Protocol(
            "Server does not support Range requests (returned 200 instead of 206). \
             This library requires strict Range semantics."
                .into(),
        ));
    }
    if status != 206 {
        return Err(FsError::Network(format!("HTTP error: {status}")));
    }
    if let Some((resp_start, _)) = content_range
        && resp_start != requested_start {
            return Err(FsError::Protocol(
                "Server returned incorrect range start".into(),
            ));
        }
    Ok(())
}

#[cfg(feature = "reqwest")]
pub(crate) struct ReqwestAsyncTransport {
    client: reqwest::Client,
}

#[cfg(feature = "reqwest")]
impl ReqwestAsyncTransport {
    pub fn new(config: &HttpConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.read_timeout)
            .connect_timeout(config.connect_timeout)
            .pool_idle_timeout(config.idle_timeout)
            .build()
            .expect("Failed to build async reqwest client");
        Self { client }
    }
}

#[cfg(feature = "reqwest")]
#[async_trait::async_trait]
impl AsyncHttp for ReqwestAsyncTransport {
    async fn get_content_length(&self, url: &str) -> Result<Option<u64>> {
        let resp = self
            .client
            .head(url)
            .send()
            .await
            .map_err(|e| FsError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Ok(None);
        }
        Ok(resp.content_length())
    }

    async fn get_range(&self, url: &str, start: u64, end: u64) -> Result<HttpResponse> {
        let resp = self
            .client
            .get(url)
            .header("Range", format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(|e| FsError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        let content_length = resp.content_length();
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let content_range = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range);

        validate_range_response(status, content_range, start, retry_after_secs)?;

        if status == 416 {
            return Ok(HttpResponse {
                data: Vec::new(),
                status,
                content_length,
                content_range,
                retry_after_secs,
            });
        }

        let data = resp
            .bytes()
            .await
            .map_err(|e| FsError::Network(e.to_string()))?
            .to_vec();

        Ok(HttpResponse {
            data,
            status,
            content_length,
            content_range,
            retry_after_secs,
        })
    }
}

#[cfg(all(not(feature = "reqwest"), feature = "curl"))]
pub(crate) struct CurlAsyncTransport {
    connect_timeout: std::time::Duration,
    read_timeout: std::time::Duration,
}

#[cfg(all(not(feature = "reqwest"), feature = "curl"))]
impl CurlAsyncTransport {
    pub fn new(config: &HttpConfig) -> Self {
        Self {
            connect_timeout: config.connect_timeout,
            read_timeout: config.read_timeout,
        }
    }

    fn do_request(
        url: String,
        head_only: bool,
        range: Option<(u64, u64)>,
        connect_timeout: std::time::Duration,
        read_timeout: std::time::Duration,
    ) -> Result<HttpResponse> {
        use ahash::{HashMap, HashMapExt};

        let mut easy = curl::easy::Easy::new();
        easy.url(&url)
            .map_err(|e| FsError::Network(e.to_string()))?;
        easy.connect_timeout(connect_timeout)
            .map_err(|e| FsError::Network(e.to_string()))?;
        easy.timeout(read_timeout)
            .map_err(|e| FsError::Network(e.to_string()))?;
        easy.follow_location(true)
            .map_err(|e| FsError::Network(e.to_string()))?;

        if head_only {
            easy.nobody(true)
                .map_err(|e| FsError::Network(e.to_string()))?;
            easy.custom_request("HEAD")
                .map_err(|e| FsError::Network(e.to_string()))?;
        }
        if let Some((s, e)) = range {
            easy.range(&format!("{s}-{e}"))
                .map_err(|e| FsError::Network(e.to_string()))?;
        }

        let mut data = Vec::new();
        let mut headers = HashMap::<String, String>::new();

        {
            let mut transfer = easy.transfer();
            transfer
                .write_function(|chunk| {
                    data.extend_from_slice(chunk);
                    Ok(chunk.len())
                })
                .map_err(|e| FsError::Network(e.to_string()))?;
            transfer
                .header_function(|header| {
                    if let Ok(line) = std::str::from_utf8(header) {
                        let line = line.trim();
                        if let Some((name, value)) = line.split_once(':') {
                            headers.insert(name.trim().to_ascii_lowercase(), value.trim().into());
                        }
                    }
                    true
                })
                .map_err(|e| FsError::Network(e.to_string()))?;
            transfer
                .perform()
                .map_err(|e| FsError::Network(e.to_string()))?;
        }

        let status = easy
            .response_code()
            .map_err(|e| FsError::Network(e.to_string()))? as u16;
        let content_length = headers
            .get("content-length")
            .and_then(|v| v.parse::<u64>().ok());
        let content_range = headers
            .get("content-range")
            .and_then(|v| parse_content_range(v));
        let retry_after_secs = headers
            .get("retry-after")
            .and_then(|v| parse_retry_after(v));

        Ok(HttpResponse {
            data,
            status,
            content_length,
            content_range,
            retry_after_secs,
        })
    }
}

#[cfg(all(not(feature = "reqwest"), feature = "curl"))]
#[async_trait::async_trait]
impl AsyncHttp for CurlAsyncTransport {
    async fn get_content_length(&self, url: &str) -> Result<Option<u64>> {
        let url = url.to_string();
        let ct = self.connect_timeout;
        let rt = self.read_timeout;
        let resp = tokio::task::spawn_blocking(move || Self::do_request(url, true, None, ct, rt))
            .await
            .map_err(|e| FsError::Network(e.to_string()))??;

        if (200..300).contains(&resp.status) {
            Ok(resp.content_length)
        } else {
            Ok(None)
        }
    }

    async fn get_range(&self, url: &str, start: u64, end: u64) -> Result<HttpResponse> {
        let url = url.to_string();
        let ct = self.connect_timeout;
        let rt = self.read_timeout;
        let resp = tokio::task::spawn_blocking(move || {
            Self::do_request(url, false, Some((start, end)), ct, rt)
        })
        .await
        .map_err(|e| FsError::Network(e.to_string()))??;

        validate_range_response(
            resp.status,
            resp.content_range,
            start,
            resp.retry_after_secs,
        )?;
        Ok(resp)
    }
}
