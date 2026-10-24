//! Sanitizes diagnostic text only; callers retain original URLs for transport/auth.
use std::borrow::Cow;
const REDACTED: &str = "[redacted]";
const SCHEMES: [&[u8]; 6] = [
    b"https://",
    b"http://",
    b"wss://",
    b"ws://",
    b"rtsps://",
    b"rtsp://",
];

fn next_url(message: &str, cursor: usize) -> Option<usize> {
    let bytes = message.as_bytes();
    for start in cursor..bytes.len() {
        if !matches!(bytes[start], b'h' | b'H' | b'w' | b'W' | b'r' | b'R') {
            continue;
        }
        if SCHEMES.iter().any(|scheme| {
            bytes[start..]
                .get(..scheme.len())
                .is_some_and(|part| part.eq_ignore_ascii_case(scheme))
        }) {
            return Some(start);
        }
    }
    None
}

fn authority_bounds(url: &str) -> (usize, usize) {
    let start = url.find("://").expect("URL scheme already matched") + 3;
    let end = url[start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| start + i);
    (start, end)
}

fn redact_url_into(output: &mut String, url: &str) {
    let (authority_start, authority_end) = authority_bounds(url);
    output.push_str(&url[..authority_start]);
    if let Some(at) = url[authority_start..authority_end].rfind('@') {
        output.push_str(REDACTED);
        output.push('@');
        output.push_str(&url[authority_start + at + 1..authority_end]);
    } else {
        output.push_str(&url[authority_start..authority_end]);
    }
    let suffix = &url[authority_end..];
    let path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
    output.push_str(&suffix[..path_end]);
    // Arbitrary/encoded/new auth keys stay private without maintaining a denylist.
    if suffix[path_end..].starts_with('?') {
        output.push('?');
        output.push_str(REDACTED);
    }
    if suffix[path_end..].contains('#') {
        output.push('#');
        output.push_str(REDACTED);
    }
}

/// No allocation unless a URL actually contains userinfo, query or fragment.
pub(crate) fn redact_urls(message: &str) -> Cow<'_, str> {
    let mut output: Option<String> = None;
    let mut cursor = 0;
    let mut copied = 0;
    while let Some(start) = next_url(message, cursor) {
        let end = message[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>'))
            .map_or(message.len(), |i| start + i);
        let url = &message[start..end];
        let (a, b) = authority_bounds(url);
        if url.contains(['?', '#']) || url[a..b].contains('@') {
            let out = output.get_or_insert_with(|| String::with_capacity(message.len()));
            out.push_str(&message[copied..start]);
            redact_url_into(out, url);
            copied = end;
        }
        cursor = end;
    }
    match output {
        Some(mut out) => {
            out.push_str(&message[copied..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_messages_and_safe_urls_are_borrowed() {
        for text in [
            "pressure updated: 101325 Pa",
            "GET https://station.local/api/boards timeout",
            "HTTPS://[::1]:443/api and wss://station/ws",
            "GPS unavailable — retrying",
        ] {
            let safe = redact_urls(text);
            assert!(matches!(safe, Cow::Borrowed(_)));
            assert_eq!(safe.as_ptr(), text.as_ptr());
        }
        assert!(matches!(
            redact_urls("wss://station/ws?token=FAKE_SECRET"),
            Cow::Owned(_)
        ));
    }

    #[test]
    fn keeps_endpoint_and_diagnostic_context_but_removes_auth_values() {
        let raw = "connect failed wss://fake-user:FAKE_PASSWORD@station.local:443/ws?token=FAKE_SESSION&epoch=2#FAKE_FRAGMENT (timeout)";
        let safe = redact_urls(raw);
        assert_eq!(
            safe,
            "connect failed wss://[redacted]@station.local:443/ws?[redacted]#[redacted] (timeout)"
        );
        for secret in [
            "fake-user",
            "FAKE_PASSWORD",
            "FAKE_SESSION",
            "FAKE_FRAGMENT",
        ] {
            assert!(!safe.contains(secret));
        }
        assert!(
            raw.contains("FAKE_SESSION"),
            "transport input must remain unchanged"
        );
    }
    #[test]
    fn masks_encoded_unknown_and_case_variant_query_keys_without_a_denylist() {
        for key in [
            "token",
            "TOKEN",
            "%74oken",
            "access_token",
            "password",
            "api_key",
            "ticket",
            "future_auth",
        ] {
            let raw = format!("https://station/api?{key}=FAKE_SECRET");
            let safe = redact_urls(&raw);
            assert_eq!(safe, "https://station/api?[redacted]");
        }
    }
    #[test]
    fn handles_multiple_urls_and_debug_wrappers() {
        assert_eq!(
            redact_urls(
                r#"request { url: "HTTPS://one/api?secret=FAKE_ONE", fallback: "wss://two/ws?ticket=FAKE_TWO" }"#
            ),
            r#"request { url: "HTTPS://one/api?[redacted]", fallback: "wss://two/ws?[redacted]" }"#
        );
    }
    #[test]
    fn preserves_ipv6_endpoints_and_strips_userinfo_without_a_query() {
        assert_eq!(
            redact_urls("rtsp://fake:FAKE_SECRET@[fe80::1]:8554/camera"),
            "rtsp://[redacted]@[fe80::1]:8554/camera"
        );
    }
    #[test]
    fn unchanged_non_url_errors_and_plain_endpoints_remain_useful() {
        for text in [
            "CAN timeout: 20ms",
            "https://station.local/api/boards",
            "GPS unavailable — retrying",
            "HTTP 403",
        ] {
            assert_eq!(redact_urls(text), text);
        }
    }
    #[test]
    fn redaction_is_idempotent_and_unicode_safe() {
        let safe =
            redact_urls("Échec https://user:FAKE_SECRET@station/ws?ticket=FAKE_TICKET — reprise");
        assert_eq!(redact_urls(&safe), safe);
    }
    #[test]
    fn fragments_and_empty_queries_are_never_echoed() {
        assert_eq!(
            redact_urls("https://station/#access_token=FAKE_FRAGMENT"),
            "https://station/#[redacted]"
        );
        assert_eq!(
            redact_urls("wss://station/ws?"),
            "wss://station/ws?[redacted]"
        );
    }
    #[test]
    fn punctuation_inside_credentials_is_not_a_url_boundary() {
        for raw in [
            "https://user:FAKE)PASSWORD@station.local/api",
            "wss://station.local/ws?token=FAKE)SECRET",
            "https://station.local/api?ticket=FAKE}SECRET",
        ] {
            let safe = redact_urls(raw);
            assert!(!safe.contains("FAKE"), "{safe}");
            assert!(!safe.contains("PASSWORD"), "{safe}");
            assert!(!safe.contains("SECRET"), "{safe}");
            assert!(safe.contains("station.local"));
        }
    }
}
