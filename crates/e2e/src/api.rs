//! Thin client over the server's management plane (`/api/...`).
//!
//! Only the endpoints the integration tests need are modelled, but they are modelled
//! faithfully: the shapes match the server's OpenAPI document for v0.3.1.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::time::sleep;
use tracing::debug;

/// Device and its one-time token, as returned by `POST /api/devices`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCredentials {
    /// Server-assigned device id (a ULID).
    pub device_id: String,
    /// Permanent device token, `wdt_...`, shown once at creation.
    pub token: String,
}

/// A device as reported by `GET /api/devices/{id}/`.
#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    /// Device id.
    pub id: String,
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// Whether a device WebSocket is attached right now.
    ///
    /// Absent from list and create responses; only `GET /api/devices/{id}/` reports it.
    #[serde(default)]
    pub online: bool,
    /// Cameras from the current registration; empty while offline.
    #[serde(default)]
    pub cameras: Vec<serde_json::Value>,
}

/// A recording stream row.
#[derive(Debug, Clone, Deserialize)]
pub struct Stream {
    /// Stream id.
    pub id: String,
    /// Owning device.
    pub device_id: String,
    /// Camera the stream records.
    pub camera_enum: u32,
    /// `active` or `completed`, among the server's statuses.
    pub status: String,
    /// RFC3339 start timestamp.
    pub started_at: String,
    /// RFC3339 end timestamp once finished.
    #[serde(default)]
    pub ended_at: Option<String>,
    /// Snapshot of resolution/fps/codecs taken when the stream was created.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// One stored chunk of a stream.
#[derive(Debug, Clone, Deserialize)]
pub struct Segment {
    /// Segment id.
    pub id: String,
    /// Owning stream.
    pub stream_id: String,
    /// Zero-based position within the stream.
    #[serde(default)]
    pub segment_seq: i64,
    /// Stored payload size in bytes.
    pub size_bytes: i64,
    /// RFC3339 creation timestamp.
    #[serde(default)]
    pub created_at: String,
}

#[derive(Deserialize)]
struct DeviceList {
    #[serde(default)]
    items: Vec<Device>,
}

#[derive(Deserialize)]
struct StreamList {
    #[serde(default)]
    items: Vec<Stream>,
}

#[derive(Deserialize)]
struct SegmentList {
    #[serde(default)]
    items: Vec<Segment>,
}

#[derive(Deserialize)]
struct CreateDevice {
    device: Device,
    token: String,
}

#[derive(Deserialize)]
struct Health {
    status: String,
}

/// Async client for the server's management plane.
pub struct ManagementApi {
    client: reqwest::Client,
    base: String,
}

impl ManagementApi {
    /// Build a client for a server base URL such as `http://127.0.0.1:18080`.
    pub fn new(base: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("building the management API client")?;
        let base = base.into();
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// Base URL without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Poll `GET /healthz` until the process answers `{"status":"ok"}`.
    pub async fn wait_healthz(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.probe_healthz().await {
                Ok(()) => return Ok(()),
                Err(error) if Instant::now() >= deadline => {
                    bail!("the server was not healthy after {timeout:?}: {error}");
                }
                Err(_) => {}
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    /// Poll `GET /api/devices` until it succeeds, which proves the DB is reachable and the
    /// embedded migrations have run.
    pub async fn wait_database(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.get_json::<DeviceList>("/api/devices").await {
                Ok(list) => {
                    debug!(devices = list.items.len(), "database ready");
                    return Ok(());
                }
                Err(error) if Instant::now() >= deadline => {
                    bail!("the database was not ready after {timeout:?}: {error}");
                }
                Err(_) => {}
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    /// One `GET /healthz` probe; the error carries a human-readable reason.
    async fn probe_healthz(&self) -> std::result::Result<(), String> {
        let response = self
            .client
            .get(self.url("/healthz"))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let health = response
            .json::<Health>()
            .await
            .map_err(|error| error.to_string())?;
        if health.status != "ok" {
            return Err(format!("status {}", health.status));
        }
        Ok(())
    }

    /// Register a device and return its id and one-time token.
    pub async fn provision_device(&self, name: &str) -> Result<DeviceCredentials> {
        let response = self
            .client
            .post(self.url("/api/devices"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .context("creating a device")?;
        let created: CreateDevice = parse_json(response, "create device").await?;
        Ok(DeviceCredentials {
            device_id: created.device.id,
            token: created.token,
        })
    }

    /// Fetch one device, including its live `online` flag.
    pub async fn device(&self, device_id: &str) -> Result<Device> {
        self.get_json(&format!("/api/devices/{device_id}/")).await
    }

    /// Poll until the device reports a live WebSocket attachment.
    pub async fn wait_online(&self, device_id: &str, timeout: Duration) -> Result<Device> {
        let deadline = Instant::now() + timeout;
        loop {
            let device = self.device(device_id).await?;
            if device.online {
                return Ok(device);
            }
            if Instant::now() >= deadline {
                bail!("device {device_id} did not come online within {timeout:?}");
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    /// Begin recording a camera; returns the created (active) stream.
    pub async fn start_recording(
        &self,
        device_id: &str,
        camera_enum: u32,
        codec: Option<&str>,
    ) -> Result<Stream> {
        let body = json!({ "camera_enum": camera_enum, "codec": codec });
        let response = self
            .client
            .post(self.url(&format!("/api/devices/{device_id}/recording/start")))
            .json(&body)
            .send()
            .await
            .context("starting a recording")?;
        parse_json(response, "start recording").await
    }

    /// Stop the camera's active stream; returns the completed stream.
    pub async fn stop_recording(&self, device_id: &str, camera_enum: u32) -> Result<Stream> {
        let body = json!({ "camera_enum": camera_enum });
        let response = self
            .client
            .post(self.url(&format!("/api/devices/{device_id}/recording/stop")))
            .json(&body)
            .send()
            .await
            .context("stopping a recording")?;
        parse_json(response, "stop recording").await
    }

    /// List a device's streams, newest first.
    pub async fn streams(&self, device_id: &str) -> Result<Vec<Stream>> {
        let list: StreamList = self
            .get_json(&format!("/api/devices/{device_id}/streams"))
            .await?;
        Ok(list.items)
    }

    /// Find one of a device's streams by id.
    pub async fn find_stream(&self, device_id: &str, stream_id: &str) -> Result<Option<Stream>> {
        Ok(self
            .streams(device_id)
            .await?
            .into_iter()
            .find(|stream| stream.id == stream_id))
    }

    /// Poll until a stream reaches `status` (for example `active` or `completed`).
    pub async fn wait_stream_status(
        &self,
        device_id: &str,
        stream_id: &str,
        status: &str,
        timeout: Duration,
    ) -> Result<Stream> {
        let deadline = Instant::now() + timeout;
        loop {
            let observed = match self.find_stream(device_id, stream_id).await? {
                Some(stream) if stream.status == status => return Ok(stream),
                Some(stream) => stream.status,
                None => "absent".to_string(),
            };
            if Instant::now() >= deadline {
                bail!("stream {stream_id} did not reach status {status:?} within {timeout:?} (last seen: {observed})");
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    /// List the stored segments of a stream, in recording order.
    pub async fn segments(&self, stream_id: &str) -> Result<Vec<Segment>> {
        let list: SegmentList = self
            .get_json(&format!("/api/streams/{stream_id}/segments"))
            .await?;
        Ok(list.items)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(self.url(path))
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        parse_json(response, &format!("GET {path}")).await
    }
}

async fn parse_json<T: DeserializeOwned>(response: reqwest::Response, what: &str) -> Result<T> {
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("reading the body of {what}"))?;
    if !status.is_success() {
        bail!("{what} failed with HTTP {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parsing the body of {what}: {body}"))
}
