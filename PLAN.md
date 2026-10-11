# SmartClass Webcam Client — Implementation Plan

Client agent for [smartclass-webcam-server](https://crazy4chicken.github.io/smartclass-webcam-server/protocol/):
4K60 (3840x2160 @ 60 fps) camera capture, hardware H.264/H.265 encoding, and streaming of
encoded frames over the device WebSocket protocol. Targets Windows, macOS, Linux, iOS and Android.

Chosen approach: **native per-platform capture/encode + a shared Rust core** (option 1 from the
feasibility study). Each OS natively exposes exactly what the protocol needs — encoded sample
buffers with timestamps — while the protocol, networking and framing logic lives in one codebase.

## Architecture

```
+-------------------+  +-------------------+  +-------------------+  +-------------------+
| iOS / macOS       |  | Android           |  | Windows           |  | Linux             |
| AVFoundation +    |  | Camera2 +         |  | Media Foundation  |  | V4L2 + VAAPI /    |
| VideoToolbox      |  | MediaCodec (NDK)  |  | (+NVENC/QSV/AMF)  |  | NVENC             |
+---------+---------+  +---------+---------+  +---------+---------+  +---------+---------+
          |  C ABI: encoded AU bytes + PTS/DTS + codec config (SPS/PPS/VPS)
          v
+----------------------------------------------------------------------------+
| Rust core (shared, all platforms)                                          |
|  - registration: GET /ws/register (device token) -> single-use ticket      |
|  - WebSocket: control (text/JSON) + recording/photo (binary) channels      |
|  - binary framing: uint32 BE header len + JSON header + raw frame bytes    |
|  - command handling: switch_camera, start/stop_recording, take_photo, ping |
|  - seq/ts tagging, 16 MiB frame budget, backpressure + drop policy         |
|  - reconnect: fresh register + attach, exponential backoff                 |
+--------------------------------------+-------------------------------------+
                                       |  FFI
          +----------------------------+-----------------------------+
          |  UI shell: Flutter (flutter_rust_bridge -> core C ABI)   |
          +----------------------------------------------------------+
```

## Decisions (settled)

- **Codec preference**: `h265` first in `supported_codec` (roughly half the 4K60 bandwidth of
  H.264, ~30 vs ~60 Mbps); `h264` announced second as the universal fallback.
- **Backpressure**: bounded frame queue; on overflow, drop delta frames until the next
  keyframe (never drop a keyframe, never block the encoder). Keeps the bitstream decodable
  at the cost of needing NALU-type inspection per frame.
- **Thermal policy**: reduce encoder bitrate only. The protocol forbids a device from
  changing `resolution`/`fps` on its own — a 1080p60 downshift would require an operator
  `switch_camera`, so the client reports thermal pressure via `status` and throttles bitrate.
- **Multi-camera**: register all cameras, capture exactly one at a time (the protocol's
  model); `switch_camera` moves the single capture pipeline. No concurrent multi-streaming.
- **UI shell**: Flutter + `flutter_rust_bridge` over the Rust core's C ABI.
- **Camera capability probing**: on first launch, enumerate each camera's supported
  resolution/fps matrix (Camera2 `StreamConfigurationMap` min-frame-duration per size on
  Android; `AVCaptureDevice.formats` frame-rate ranges on iOS/macOS), then validate every
  candidate pair by actually creating a capture session (vendors misreport). The verified
  matrix is persisted (keyed by camera unique id) and reused on subsequent launches without
  re-probing; a re-verify option (CLI flag / UI action) forces full enumeration+validation
  again. The registration lists always come from the verified cache.
- **FFI linking strategy**: the core crate builds as `crate-type = ["staticlib", "cdylib",
  "rlib"]`. iOS/macOS/Windows/Linux link the core **statically** (`.a`/`.lib` into the app or
  runner binary via podspec/CMake); Android is the exception — dart:ffi can only load shared
  libraries, so Android ships `libscwc_core.so` in `jniLibs/<abi>/`. Bindings via
  flutter_rust_bridge v2 (async events over FRB streams / Dart_PostCObject). All dependencies
  are pure Rust + rustls, so static linking carries no GPL or dynamic-runtime baggage; the
  openh264 dependency is M1-only and must not leak into the production core.

### Still open

- Core crate layout (single crate vs workspace split: protocol / net / media / ffi)
- Byte-stream format handed to the core (Annex B preferred for simplicity; AVCC possible)
- Windows encoder MFT probe order (NVENC / QSV / AMF / inbox) and the Win10 software-encode
  fallback behavior (reject 4K60 vs allow degraded)
- Concrete bitrate/GOP defaults (starting point: HEVC 30 Mbps, H.264 53 Mbps, 2 s keyframes)
- M1 test target: real smartclass-webcam-server instance vs a local mock
- Token provisioning flow (config file vs env vs first-run onboarding)

## Components

### 1. Rust core (`core/`)

Single crate (or workspace) exposing a C ABI. Owns everything that is not camera-specific.

- **Registration**: `GET /ws/register` with `Authorization: Bearer wdt_...`; body announces every
  camera (`camera_enum` 0..n-1, current `resolution`/`fps`, `supported_resolutions`,
  `supported_framerates`, `supported_codec` — closed set `h264,h265,mjpeg,mpeg4,vp8,vp9,av1`,
  first entry = preferred). Parse `expires_at` with full RFC 3339 (sub-second precision).
- **WebSocket**: attach `GET /ws/device/{ticket}` within the 60 s ticket TTL. Tickets are
  single-use: any close → re-register → re-attach. Exponential backoff on reconnect; a newer
  connection of the same device may kill this one without a close handshake.
- **Control channel**: text JSON envelopes. Handle `switch_camera`, `start_recording`,
  `stop_recording`, `take_photo`, `ping`; send `ack` (echo command `id`, `ok`/`error`),
  `pong`, `status`, `error`. Keepalive: answer protocol pings and application `ping` —
  60 s of silence closes the connection.
- **Media framing**: per frame — 4-byte big-endian header length, UTF-8 JSON header
  (`channel: "recording"`, `type: "frame"`, `payload: {camera_enum, stream_id, seq, ts}`),
  then raw encoded bytes. Whole frame must stay < 16 MiB (4K60 H.264 averages 50-140 KB
  per frame, far below the cap).
- **Sequencing**: `seq` monotonically increasing per stream (server stores arrival order,
  never reorders); `ts` RFC3339Nano per frame so segment durations stay honest when frames drop.
- **Backpressure**: bounded frame queue between encoder callback and WS writer; on overflow,
  drop delta frames until the next keyframe (never block the encoder, never drop a keyframe).
  Requires NALU-type inspection per frame; drop counters surfaced via `status`.
- **Photos**: JPEG only; on `take_photo` capture a still, send on the `photo` channel with the
  command's `request_id`.
- **Config**: device id + `wdt_` token, server base URL, per-camera defaults. Token is a
  credential — OS keychain/keystore storage, never in plaintext config committed to the repo.

### 2. Platform capture/encode layers (`platform/`)

Thin native shims (~500-1500 LOC each) behind one C ABI:

```
init(config) -> handle
start(handle, callbacks: onEncodedFrame(bytes, pts, isKey, codecConfig), onError)
setParameters(handle, cameraId, width, height, fps, codec, bitrate)
stop(handle) / destroy(handle)
```

| Platform | Capture | Encoder | Notes |
|---|---|---|---|
| iOS / macOS | `AVCaptureSession` | `VTCompressionSession` | 4K60 on iPhone 8+ (A11); output `CMSampleBuffer` carries NALUs + PTS; SPS/PPS/VPS from sample-buffer attachments. ~500-1000 LOC Swift/ObjC++. |
| Android | Camera2 (NDK `libcamera2ndk`) | `AMediaCodec` `video/avc` / `video/hevc` | 4K60 reliable on Snapdragon 845 / Exynos 9810 and newer; `CODEC_CONFIG` buffer carries SPS/PPS/VPS. Thermal pressure (API 29 `PowerManager`) triggers bitrate reduction only — resolution/fps may not change without `switch_camera`. |
| Windows | Media Foundation `IMFMediaSource` (UVC) | Encoder `IMFTransform` | Caveat: inbox H.264 encoder MFT is software-only before Windows 11 24H2 — on Win10 use NVENC / Intel QSV / AMD AMF MFTs. Riskiest platform for HW 4K60. |
| Linux | V4L2 (MJPEG from 4K60 UVC device) | VAAPI `vah264enc/vah265enc` or NVENC | Most manual work (~1000-1500 LOC). |

Encoded output format: one access unit per callback, Annex B or AVCC announced up front;
codec-config (SPS/PPS/VPS) delivered out-of-band and prepended before keyframes as required.

### 3. UI shell (Flutter, later milestone)

Minimal operator UI (server URL, token provisioning, camera list, live status) in Flutter,
talking to the core through `flutter_rust_bridge` over the C ABI. The core must run headless
first; the UI adds no capture/encode logic of its own.

## Protocol obligations checklist

- [ ] Re-announce the full camera set on every (re)registration
- [ ] Attach before ticket expiry; on 404/409 restart at registration
- [ ] Never reuse a ticket; any close → full re-register
- [ ] Answer WS pings and application `ping` (60 s silence = disconnect)
- [ ] `ack` every command, echoing its `id`; failures via `ok: false` + `error`
- [ ] Binary frames: `uint32 BE` header length (1..65536) + JSON header + raw bytes, < 16 MiB total
- [ ] Tag frames with the `stream_id` from `start_recording`, monotonic `seq`, RFC3339Nano `ts`
- [ ] Push media only for announced streams; stop on acknowledged `stop_recording`
- [ ] Photos: JPEG, `content_type: image/jpeg` or absent, echo `request_id`
- [ ] Frames are dropped silently server-side on any mismatch — client must self-verify
      (no protocol feedback for discarded media)
- [ ] Expect abrupt replacement by a newer connection; reconnect with backoff
- [ ] `switch_camera` only to announced `supported_resolutions`/`supported_framerates`; on
      `ok: true` the server rewrites the camera's live parameters
- [ ] Ignore unknown message types/channels (forward compatibility)

## Hardware requirements

- **Desktop camera**: true 4K60 UVC is rare (Elgato Facecam Pro / Facecam 4K class, USB 3.0).
  Most "4K" webcams are 4K30 (MX Brio, OBSBOT Tiny 4K, Insta360 Link). USB 2.0 cannot carry
  4K60 in any format. Robust classroom alternative: mirrorless/PTZ camera -> HDMI capture card
  (Cam Link 4K) presenting as UVC.
- **Phones**: iPhone 8+ / Snapdragon 845+ flagships capture 4K60, but sustained multi-hour 4K60
  throttles thermally — the mitigation is encoder bitrate reduction (resolution/fps are
  operator-controlled per protocol), plus `status` reporting of thermal pressure.
- **Uplink**: HEVC 4K60 ≈ 25-35 Mbps, H.264 4K60 ≈ 53-68 Mbps (YouTube guidance) for
  talking-head content. Wired GbE or Wi-Fi 5 GHz required; adaptive bitrate on degradation.

## Milestones

1. **M1 — Core protocol, headless**: Rust core with a synthetic frame source (test pattern
   through a software x264 encode); proves register/attach/control/framing/reconnect against a
   real server; segments readable via the management API.
2. **M2 — Desktop capture**: macOS (VideoToolbox) first, then Windows (MF + vendor MFTs) and
   Linux (V4L2 + VAAPI). Real 4K60 webcam end-to-end.
3. **M3 — Mobile**: iOS (AVFoundation/VideoToolbox), then Android (Camera2/MediaCodec) with
   thermal-aware bitrate control.
4. **M4 — Hardening**: backpressure/drop tuning at 50+ Mbps, reconnect storms, soak tests
   (multi-hour 4K60), photo path, `switch_camera` matrix.
5. **M5 — Flutter UI shell + packaging**: provisioning flow, installers/bundles per platform.

## Non-goals

- Live view / playback (the server has no live-view endpoint; retrieval is via management API)
- Server-side changes — the codec vocabulary and framing are fixed by the protocol
- Local muxed recording alongside streaming (can be added later in the core)
