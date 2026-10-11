//! Registration: the `GET /ws/register` exchange that mints a single-use WebSocket ticket.

use std::time::Duration;

use scwc_protocol::{RegisterRequest, RegisterResponse};

use crate::CoreError;
use crate::config::ClientConfig;

/// Path of the registration endpoint, appended to [`ClientConfig::server`].
const REGISTER_PATH: &str = "/ws/register";

/// Longest a single registration attempt may take before it is retried.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Registers with the server and returns the freshly minted ticket.
///
/// The request is a `GET` with a JSON body, as the protocol demands, authenticated by the
/// device token. Every non-success status is mapped to a typed [`CoreError`] so the
/// supervisor can decide whether to retry.
pub(crate) async fn register(
    client: &reqwest::Client,
    config: &ClientConfig,
) -> Result<RegisterResponse, CoreError> {
    let url = format!("{}{REGISTER_PATH}", config.server.trim_end_matches('/'));
    let body = RegisterRequest {
        device_id: config.device_id.clone(),
        cameras: config.cameras.clone(),
    };
    let body = serde_json::to_vec(&body)?;

    let response = client
        .get(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", config.device_token),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let detail = problem_detail(response).await;
        return Err(CoreError::from_status(status.as_u16(), detail));
    }

    let body = response.text().await?;
    let registration: RegisterResponse = serde_json::from_str(&body)?;
    Ok(registration)
}

/// Builds the WebSocket URL for `path` from the configured HTTP base URL.
///
/// `http` becomes `ws` and `https` becomes `wss`; a base URL that already uses a WebSocket
/// scheme is passed through. A path prefix on the base URL, as used behind a reverse
/// proxy, is preserved in front of `path`.
pub(crate) fn ws_url(base: &str, path: &str) -> Result<String, CoreError> {
    let Some((scheme, rest)) = split_scheme(base) else {
        return Err(CoreError::Config(format!(
            "server URL {base:?} must start with http://, https://, ws:// or wss://"
        )));
    };
    let secure = scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("wss");

    let rest = rest.trim_end_matches('/');
    let (authority, prefix) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(CoreError::Config(format!(
            "server URL {base:?} has no host"
        )));
    }

    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let scheme = if secure { "wss" } else { "ws" };
    Ok(format!("{scheme}://{authority}{prefix}{path}"))
}

/// Splits a URL into its scheme and the remainder, rejecting unknown schemes.
fn split_scheme(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let known = ["http", "https", "ws", "wss"]
        .iter()
        .any(|known| scheme.eq_ignore_ascii_case(known));

    (known && !rest.is_empty()).then_some((scheme, rest))
}

/// Extracts the useful part of a problem+json (or plain-text) error body.
async fn problem_detail(response: reqwest::Response) -> String {
    let status = response.status();
    let fallback = format!("HTTP {status}");

    let Ok(body) = response.text().await else {
        return fallback;
    };
    let body = body.trim();
    if body.is_empty() {
        return fallback;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["detail", "title", "message"] {
            if let Some(message) = value.get(key).and_then(serde_json::Value::as_str) {
                if !message.trim().is_empty() {
                    return message.to_owned();
                }
            }
        }
    }

    const MAX_DETAIL: usize = 512;
    if body.len() <= MAX_DETAIL {
        body.to_owned()
    } else {
        let mut end = MAX_DETAIL;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &body[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::ws_url;

    #[test]
    fn derives_websocket_urls_from_the_base_url() {
        assert_eq!(
            ws_url("https://smartclass.example.edu", "/ws/device/abc").ok(),
            Some("wss://smartclass.example.edu/ws/device/abc".to_owned())
        );
        assert_eq!(
            ws_url("http://10.0.0.5:8080/", "/ws/device/abc").ok(),
            Some("ws://10.0.0.5:8080/ws/device/abc".to_owned())
        );
        assert_eq!(
            ws_url("https://example.edu/gateway", "ws/device/abc").ok(),
            Some("wss://example.edu/gateway/ws/device/abc".to_owned())
        );
    }

    #[test]
    fn rejects_unusable_base_urls() {
        for base in ["", "smartclass.example.edu", "ftp://host", "https://"] {
            assert!(ws_url(base, "/ws/device/abc").is_err(), "{base:?} must fail");
        }
    }
}
