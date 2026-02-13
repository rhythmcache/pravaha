#![allow(dead_code)]
use crate::core::{FsError, Result};
use crate::http::HttpConfig;
use ahash::{HashMap, HashMapExt};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub struct HttpResponse {
    pub data: Vec<u8>,
    pub status: u16,
    pub content_length: Option<u64>,
    pub content_range: Option<(u64, u64)>,
}

impl HttpResponse {
    pub fn new(
        data: Vec<u8>,
        status: u16,
        content_length: Option<u64>,
        content_range: Option<(u64, u64)>,
    ) -> Self {
        Self {
            data,
            status,
            content_length,
            content_range,
        }
    }
}

/// internal blocking transport trait.
pub trait BlockingHttp: Send + Sync {
    fn get_content_length(&self, url: &str) -> Result<Option<u64>>;
    fn get_range(&self, url: &str, start: u64, end: u64) -> Result<HttpResponse>;
}

#[cfg(all(not(feature = "reqwest"), not(feature = "curl")))]
compile_error!("Enable either `curl` (default) or `reqwest` feature.");

pub(crate) fn build_default_transport(config: &HttpConfig) -> Arc<dyn BlockingHttp> {
    #[cfg(feature = "reqwest")]
    {
        Arc::new(ReqwestBlockingTransport::new(config))
    }
    #[cfg(all(not(feature = "reqwest"), feature = "curl"))]
    {
        Arc::new(CurlBlockingTransport::new(config))
    }
}

fn parse_content_range(header: &str) -> Option<(u64, u64)> {
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 2 || parts[0] != "bytes" {
        return None;
    }

    let range_part = parts[1].split('/').next()?;
    let mut range_iter = range_part.split('-');

    let start = range_iter.next()?.parse::<u64>().ok()?;
    let end = range_iter.next()?.parse::<u64>().ok()?;

    Some((start, end))
}

#[cfg(feature = "reqwest")]
struct ReqwestBlockingTransport {
    client: reqwest::blocking::Client,
}

#[cfg(feature = "reqwest")]
impl ReqwestBlockingTransport {
    fn new(config: &HttpConfig) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(config.read_timeout)
            .connect_timeout(config.connect_timeout)
            .pool_idle_timeout(config.idle_timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self { client }
    }
}

#[cfg(feature = "reqwest")]
impl BlockingHttp for ReqwestBlockingTransport {
    fn get_content_length(&self, url: &str) -> Result<Option<u64>> {
        let response = self
            .client
            .head(url)
            .send()
            .map_err(|e| FsError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Ok(None);
        }

        Ok(response.content_length())
    }

    fn get_range(&self, url: &str, start: u64, end: u64) -> Result<HttpResponse> {
        let range_header = format!("bytes={}-{}", start, end);

        let response = self
            .client
            .get(url)
            .header("Range", range_header)
            .send()
            .map_err(|e| FsError::Network(e.to_string()))?;

        let status = response.status().as_u16();
        let content_length = response.content_length();

        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range);

        if status == 416 {
            return Ok(HttpResponse {
                data: Vec::new(),
                status,
                content_length,
                content_range,
            });
        }

        if status == 200 {
            return Err(FsError::Protocol(
                "Server does not support Range requests (returned 200 instead of 206). \
                 This library requires strict Range semantics."
                    .into(),
            ));
        }

        if status != 206 {
            return Err(FsError::Network(format!("HTTP error: {}", status)));
        }

        if let Some((resp_start, _)) = content_range
            && resp_start != start
        {
            return Err(FsError::Protocol(
                "Server returned incorrect range start".into(),
            ));
        }

        let data = response
            .bytes()
            .map_err(|e| FsError::Network(e.to_string()))?
            .to_vec();

        Ok(HttpResponse {
            data,
            status,
            content_length,
            content_range,
        })
    }
}

#[cfg(all(not(feature = "reqwest"), feature = "curl"))]
struct CurlBlockingTransport {
    connect_timeout: Duration,
    read_timeout: Duration,
}

#[cfg(all(not(feature = "reqwest"), feature = "curl"))]
impl CurlBlockingTransport {
    fn new(config: &HttpConfig) -> Self {
        Self {
            connect_timeout: config.connect_timeout,
            read_timeout: config.read_timeout,
        }
    }

    fn request(
        &self,
        url: &str,
        head_only: bool,
        range: Option<(u64, u64)>,
    ) -> Result<HttpResponse> {
        let mut easy = curl::easy::Easy::new();
        easy.url(url).map_err(|e| FsError::Network(e.to_string()))?;
        easy.connect_timeout(self.connect_timeout)
            .map_err(|e| FsError::Network(e.to_string()))?;
        easy.timeout(self.read_timeout)
            .map_err(|e| FsError::Network(e.to_string()))?;
        easy.follow_location(true)
            .map_err(|e| FsError::Network(e.to_string()))?;

        if head_only {
            easy.nobody(true)
                .map_err(|e| FsError::Network(e.to_string()))?;
            easy.custom_request("HEAD")
                .map_err(|e| FsError::Network(e.to_string()))?;
        }

        if let Some((start, end)) = range {
            easy.range(&format!("{start}-{end}"))
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
            .and_then(|value| parse_content_range(value));

        Ok(HttpResponse {
            data,
            status,
            content_length,
            content_range,
        })
    }
}

#[cfg(all(not(feature = "reqwest"), feature = "curl"))]
impl BlockingHttp for CurlBlockingTransport {
    fn get_content_length(&self, url: &str) -> Result<Option<u64>> {
        let response = self.request(url, true, None)?;
        if (200..300).contains(&response.status) {
            return Ok(response.content_length);
        }
        Ok(None)
    }

    fn get_range(&self, url: &str, start: u64, end: u64) -> Result<HttpResponse> {
        let response = self.request(url, false, Some((start, end)))?;

        if response.status == 416 {
            return Ok(HttpResponse {
                data: Vec::new(),
                status: response.status,
                content_length: response.content_length,
                content_range: response.content_range,
            });
        }

        if response.status == 200 {
            return Err(FsError::Protocol(
                "Server does not support Range requests (returned 200 instead of 206). \
                 This library requires strict Range semantics."
                    .into(),
            ));
        }

        if response.status != 206 {
            return Err(FsError::Network(format!("HTTP error: {}", response.status)));
        }

        if let Some((resp_start, _)) = response.content_range
            && resp_start != start
        {
            return Err(FsError::Protocol(
                "Server returned incorrect range start".into(),
            ));
        }

        Ok(response)
    }
}
