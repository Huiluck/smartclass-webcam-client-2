//! End-to-end tests: a real PostgreSQL, a real smartclass-webcam-server, a real client.
//!
//! Run with `cargo test -p scwc-e2e`. The first run downloads the PostgreSQL distribution,
//! clones and builds the server, and compiles the client; later runs reuse `.cache` and are
//! a few seconds of process startup.

use std::time::Duration;

use scwc_e2e::{TestEnv, build_cli, init_tracing};

/// Resolution announced at registration (4K).
const RESOLUTION: &str = "3840x2160";
/// Frame rate announced at registration.
const FPS: u32 = 60;
/// Encoder bitrate in kbps.
const BITRATE_KBPS: u32 = 20000;
/// Bound on every assertion against the management API.
const ASSERT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the client streams before the stream is stopped.
const STREAM_DURATION: Duration = Duration::from_secs(15);

/// Registers a device, streams a synthetic test pattern, and verifies the result through the
/// management plane: the stream goes active, then completed with at least one non-empty
/// segment.
#[tokio::test(flavor = "multi_thread")]
async fn streams_synthetic_video() -> anyhow::Result<()> {
    init_tracing();

    let env = TestEnv::start().await?;
    let credentials = env.provision_device().await?;
    let binary = build_cli()?;
    let mut client = env.spawn_client(
        &binary,
        &credentials,
        RESOLUTION,
        FPS,
        BITRATE_KBPS,
        Some("h264"),
    )?;

    // The server refuses to start a recording for an offline device, so wait for the attach.
    let device = env
        .api()
        .wait_online(&credentials.device_id, ASSERT_TIMEOUT)
        .await?;
    assert!(device.online, "device never came online");
    assert!(
        !device.cameras.is_empty(),
        "the client registered no cameras: {device:?}"
    );

    let stream = env
        .api()
        .start_recording(&credentials.device_id, 0, Some("h264"))
        .await?;
    assert_eq!(stream.camera_enum, 0, "unexpected camera: {stream:?}");
    assert_eq!(stream.status, "active", "unexpected status: {stream:?}");

    // Confirm active through the management API, not just the create response.
    env.api()
        .wait_stream_status(&credentials.device_id, &stream.id, "active", ASSERT_TIMEOUT)
        .await?;

    tokio::time::sleep(STREAM_DURATION).await;

    let active = env
        .api()
        .find_stream(&credentials.device_id, &stream.id)
        .await?
        .unwrap_or_else(|| panic!("stream {} vanished while streaming", stream.id));
    assert_eq!(
        active.status, "active",
        "stream ended on its own: {active:?}"
    );

    let stopped = env
        .api()
        .stop_recording(&credentials.device_id, 0)
        .await?;
    assert_eq!(stopped.id, stream.id, "stopped the wrong stream");
    assert_eq!(stopped.status, "completed", "unexpected status: {stopped:?}");

    let segments = env.api().segments(&stream.id).await?;
    assert!(
        !segments.is_empty(),
        "stream {} recorded no segments",
        stream.id
    );
    assert!(
        segments.iter().any(|segment| segment.size_bytes > 0),
        "every segment is empty: {segments:?}"
    );

    client.stop();
    Ok(())
}

/// Streams from the host's first real camera (1080p30 H.264) through the full pipeline.
///
/// Ignored by default: needs a physical camera plus camera (TCC) permission for the test
/// process. Run with `cargo test -p scwc-e2e -- --ignored streams_camera_video`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a physical camera and camera permission"]
async fn streams_camera_video() -> anyhow::Result<()> {
    use scwc_e2e::{ClientOptions, spawn_cli};

    init_tracing();

    let env = TestEnv::start().await?;
    let credentials = env.provision_device().await?;
    let binary = build_cli()?;

    let base_url = env.api().base_url().to_string();
    let options = ClientOptions {
        server_url: &base_url,
        device_id: &credentials.device_id,
        token: &credentials.token,
        resolution: "1920x1080",
        fps: 30,
        bitrate_kbps: BITRATE_KBPS,
        codec: Some("h264"),
        extra_args: vec!["--source".into(), "camera".into()],
    };
    let log = std::env::temp_dir().join("scwc-e2e-camera-client.log");
    let mut client = spawn_cli(&binary, &options, &log)?;

    let device = env
        .api()
        .wait_online(&credentials.device_id, ASSERT_TIMEOUT)
        .await?;
    assert!(device.online, "device never came online");

    let stream = env
        .api()
        .start_recording(&credentials.device_id, 0, Some("h264"))
        .await?;
    assert_eq!(stream.status, "active", "unexpected status: {stream:?}");

    tokio::time::sleep(STREAM_DURATION).await;

    let stopped = env
        .api()
        .stop_recording(&credentials.device_id, 0)
        .await?;
    assert_eq!(stopped.status, "completed", "unexpected status: {stopped:?}");

    let segments = env.api().segments(&stream.id).await?;
    assert!(!segments.is_empty(), "no segments recorded from the camera");
    assert!(
        segments.iter().any(|segment| segment.size_bytes > 0),
        "all segments are empty: {segments:?}"
    );

    client.stop();
    Ok(())
}

/// Kills the server mid-recording and checks that the client re-registers once it is back.
///
/// Ignored by default: the reconnect depends on the client's backoff timing, which makes the
/// test slow and, occasionally, flaky.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "flaky-prone: depends on the client noticing an abrupt server restart"]
async fn reconnects_after_server_restart() -> anyhow::Result<()> {
    init_tracing();

    let mut env = TestEnv::start().await?;
    let credentials = env.provision_device().await?;
    let binary = build_cli()?;
    let mut client = env.spawn_client(
        &binary,
        &credentials,
        RESOLUTION,
        FPS,
        BITRATE_KBPS,
        Some("h264"),
    )?;

    env.api()
        .wait_online(&credentials.device_id, ASSERT_TIMEOUT)
        .await?;

    env.restart_server().await?;

    let device = env
        .api()
        .wait_online(&credentials.device_id, Duration::from_secs(60))
        .await?;
    assert!(
        device.online,
        "the client did not re-register after the server restart"
    );

    client.stop();
    Ok(())
}
