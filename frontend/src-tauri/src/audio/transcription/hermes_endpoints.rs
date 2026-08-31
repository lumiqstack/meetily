// audio/transcription/hermes_endpoints.rs
//
// URL derivation for the hermes proxy. Both Gemini transports are configured
// from a single stored base URL that already includes the gateway path
// segment, e.g.
//
//     https://<host>/google-transcribe
//
// from which:
//     batch → https://<host>/google-transcribe/v1/transcriptions
//     live  → wss://<host>/google-transcribe/v1/live
//
// The real host is a per-user setting and is never hardcoded here.
//
// The gateway path segment is never synthesized here. It is part of the base
// the user configured, so it cannot be appended a second time.

const BATCH_PATH: &str = "/v1/transcriptions";
const LIVE_PATH: &str = "/v1/live";

/// Trim whitespace and trailing slashes without otherwise rewriting the base.
fn normalize_base(base_url: &str) -> &str {
    base_url.trim().trim_end_matches('/')
}

/// Append `suffix` unless the base already ends with it, so that passing an
/// already-resolved endpoint back through is a no-op.
fn with_suffix(base: &str, suffix: &str) -> String {
    if base.ends_with(suffix) {
        base.to_string()
    } else {
        format!("{}{}", base, suffix)
    }
}

/// Resolve the multipart batch transcription endpoint.
pub fn resolve_rest_endpoint(base_url: &str) -> Result<String, String> {
    let base = normalize_base(base_url);
    if base.is_empty() {
        return Err("Gemini transcription base URL is not configured.".to_string());
    }

    let endpoint = with_suffix(base, BATCH_PATH);
    reqwest::Url::parse(&endpoint)
        .map_err(|e| format!("Invalid Gemini transcription URL '{}': {}", endpoint, e))?;
    Ok(endpoint)
}

/// Resolve the live WebSocket endpoint, upgrading the scheme to `wss`/`ws`.
pub fn resolve_live_endpoint(base_url: &str) -> Result<String, String> {
    let base = normalize_base(base_url);
    if base.is_empty() {
        return Err("Gemini transcription base URL is not configured.".to_string());
    }

    let with_path = with_suffix(base, LIVE_PATH);

    let mut url = reqwest::Url::parse(&with_path)
        .map_err(|e| format!("Invalid Gemini live URL '{}': {}", with_path, e))?;

    // Keep an explicitly-configured ws/wss base as-is; upgrade http(s).
    let ws_scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(format!(
                "Unsupported scheme '{}' for the Gemini live endpoint; expected http(s) or ws(s).",
                other
            ))
        }
    };

    url.set_scheme(ws_scheme)
        .map_err(|_| format!("Could not set scheme '{}' on '{}'", ws_scheme, with_path))?;

    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://gateway.example.ts.net/google-transcribe";

    #[test]
    fn derives_both_endpoints_from_the_configured_base() {
        assert_eq!(
            resolve_rest_endpoint(BASE).unwrap(),
            "https://gateway.example.ts.net/google-transcribe/v1/transcriptions"
        );
        assert_eq!(
            resolve_live_endpoint(BASE).unwrap(),
            "wss://gateway.example.ts.net/google-transcribe/v1/live"
        );
    }

    #[test]
    fn gateway_segment_is_never_duplicated() {
        // The regression this guards: synthesizing "/google-transcribe"
        // instead of treating it as part of the configured base.
        for url in [
            resolve_rest_endpoint(BASE).unwrap(),
            resolve_live_endpoint(BASE).unwrap(),
        ] {
            assert_eq!(url.matches("google-transcribe").count(), 1, "{}", url);
        }
    }

    #[test]
    fn resolution_is_idempotent() {
        let rest = resolve_rest_endpoint(BASE).unwrap();
        assert_eq!(resolve_rest_endpoint(&rest).unwrap(), rest);

        let live = resolve_live_endpoint(BASE).unwrap();
        assert_eq!(resolve_live_endpoint(&live).unwrap(), live);
    }

    #[test]
    fn trailing_slashes_and_whitespace_are_tolerated() {
        for base in [
            "  https://host.ts.net/google-transcribe  ",
            "https://host.ts.net/google-transcribe/",
            "https://host.ts.net/google-transcribe///",
        ] {
            assert_eq!(
                resolve_rest_endpoint(base).unwrap(),
                "https://host.ts.net/google-transcribe/v1/transcriptions"
            );
            assert_eq!(
                resolve_live_endpoint(base).unwrap(),
                "wss://host.ts.net/google-transcribe/v1/live"
            );
        }
    }

    #[test]
    fn plain_http_downgrades_to_ws_for_local_gateways() {
        assert_eq!(
            resolve_live_endpoint("http://127.0.0.1:8080/google-transcribe").unwrap(),
            "ws://127.0.0.1:8080/google-transcribe/v1/live"
        );
    }

    #[test]
    fn explicit_ws_base_is_preserved() {
        assert_eq!(
            resolve_live_endpoint("wss://host.ts.net/google-transcribe").unwrap(),
            "wss://host.ts.net/google-transcribe/v1/live"
        );
    }

    #[test]
    fn a_base_without_the_gateway_segment_is_left_alone() {
        // Some deployments may mount the gateway at the root; we must not
        // invent a path segment for them either.
        assert_eq!(
            resolve_rest_endpoint("https://host.ts.net").unwrap(),
            "https://host.ts.net/v1/transcriptions"
        );
    }

    #[test]
    fn empty_base_is_rejected() {
        assert!(resolve_rest_endpoint("   ").is_err());
        assert!(resolve_live_endpoint("").is_err());
    }

    #[test]
    fn unsupported_scheme_is_rejected() {
        assert!(resolve_live_endpoint("ftp://host.ts.net/google-transcribe").is_err());
    }
}
