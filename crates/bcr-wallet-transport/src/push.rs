use crate::error::Result;
use bitcoin::{
    base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD},
    hashes::{Hash, sha256},
};
use nostr::{
    event::{EventBuilder, EventId, FinalizeEvent, Kind, Tag},
    key::Keys,
    types::{RelayUrl, Timestamp, Url},
};
use serde::{Deserialize, Serialize};

/// Path of the relay endpoint that registers and removes push tokens
pub const PUSH_PATH: &str = "push";

/// What a relay push carries: the stable id of the stored event and where to fetch it
///
/// # Example
/// ```json
/// {"event_id": "<hex>", "relay_url": "wss://relay.example.com", "kind": 1059}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPayload {
    pub event_id: EventId,
    pub relay_url: String,
    #[serde(deserialize_with = "kind_from_number_or_string")]
    pub kind: u16,
}

/// FCM data maps carry only strings, so the relay's `kind` arrives as `"1059"` there
fn kind_from_number_or_string<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<u16, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Kind {
        Number(u16),
        Text(String),
    }
    match Kind::deserialize(d)? {
        Kind::Number(kind) => Ok(kind),
        Kind::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
}

/// Platform a push token belongs to; a key holds at most one token per platform
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Android,
    Ios,
}

/// Body of `PUT /push` and `DELETE /push`
///
/// # Example
/// ```json
/// {"platform": "android", "token": "fcm-registration-token"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRequest {
    pub platform: Platform,
    pub token: String,
}

/// HTTP method of a device registration request
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMethod {
    Put,
    Delete,
}

impl DeviceMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceMethod::Put => "PUT",
            DeviceMethod::Delete => "DELETE",
        }
    }
}

/// Body and `Authorization` header value of one signed device registration request
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedDeviceRequest {
    pub body: String,
    pub authorization: String,
}

/// The absolute URL a NIP-98 auth event for device registration names in its `u` tag
pub fn push_url(host_url: &Url) -> Option<Url> {
    if host_url.cannot_be_a_base() {
        return None;
    }
    let mut url = host_url.clone();
    let path = format!("{}/{PUSH_PATH}", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Some(url)
}

/// The HTTP origin of the push endpoint served beside a relay: `wss` maps to `https`, `ws` to `http`
pub fn relay_host_url(relay: &RelayUrl) -> Option<Url> {
    let mut url = Url::parse(relay.as_str()).ok()?;
    let scheme = if url.scheme() == "wss" {
        "https"
    } else {
        "http"
    };
    url.set_scheme(scheme).ok()?;
    Some(url)
}

/// Signs a device registration request as a NIP-98 event (kind 27235) with `keys`; the relay
/// accepts it for `url` and `method` within 60 seconds of `now`.
pub fn sign_device_request(
    keys: &Keys,
    host_url: &Url,
    method: DeviceMethod,
    request: &DeviceRequest,
    now: Timestamp,
) -> Result<SignedDeviceRequest> {
    let url = push_url(host_url).ok_or_else(|| {
        crate::error::Error::Network(format!("invalid relay host url {host_url}"))
    })?;
    let body = serde_json::to_string(request)?;
    let payload_hash = sha256::Hash::hash(body.as_bytes()).to_string();
    let event = EventBuilder::new(Kind::HttpAuth, "")
        .tag(Tag::parse(["u", url.as_str()])?)
        .tag(Tag::parse(["method", method.as_str()])?)
        .tag(Tag::parse(["payload", payload_hash.as_str()])?)
        .custom_created_at(now)
        .finalize(keys)?;
    let token = BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&event)?);
    Ok(SignedDeviceRequest {
        body,
        authorization: format!("Nostr {token}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{event::Event, key::PublicKey};

    const WINDOW_SECS: u64 = 60;

    #[derive(Deserialize)]
    struct Fixture {
        pubkey: PublicKey,
        secret_key: String,
        host_url: Url,
        now: u64,
        requests: Vec<FixtureRequest>,
    }

    #[derive(Deserialize)]
    struct FixtureRequest {
        method: String,
        body: String,
        url: Url,
        authorization: String,
        event: Event,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../fixtures/push_auth.json")).unwrap()
    }

    fn method_of(name: &str) -> DeviceMethod {
        match name {
            "PUT" => DeviceMethod::Put,
            "DELETE" => DeviceMethod::Delete,
            other => panic!("unexpected method {other}"),
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Rejection {
        Malformed,
        Url,
        Method,
        TimeWindow,
        Payload,
        Signature,
    }

    /// The checks the relay applies to a registration request
    fn relay_checks(
        authorization: &str,
        url: &Url,
        method: &str,
        body: &str,
        now: u64,
    ) -> std::result::Result<PublicKey, Rejection> {
        let token = authorization
            .strip_prefix("Nostr ")
            .ok_or(Rejection::Malformed)?;
        let json = BASE64_URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| Rejection::Malformed)?;
        let event: Event = serde_json::from_slice(&json).map_err(|_| Rejection::Malformed)?;
        let tag = |name: &str| {
            event
                .tags
                .iter()
                .find(|t| t.kind() == name)
                .and_then(|t| t.content())
        };
        if event.kind != Kind::HttpAuth {
            return Err(Rejection::Malformed);
        }
        if tag("u").and_then(|u| Url::parse(u).ok()).as_ref() != Some(url) {
            return Err(Rejection::Url);
        }
        if tag("method") != Some(method) {
            return Err(Rejection::Method);
        }
        if event.created_at.as_secs().abs_diff(now) > WINDOW_SECS {
            return Err(Rejection::TimeWindow);
        }
        let hash = sha256::Hash::hash(body.as_bytes()).to_string();
        if tag("payload") != Some(hash.as_str()) {
            return Err(Rejection::Payload);
        }
        event.verify().map_err(|_| Rejection::Signature)?;
        Ok(event.pubkey)
    }

    fn sign(fixture: &Fixture, host: &Url, method: &str, created_at: u64) -> SignedDeviceRequest {
        let keys = Keys::parse(&fixture.secret_key).unwrap();
        let request: DeviceRequest = serde_json::from_str(&fixture.requests[0].body).unwrap();
        sign_device_request(
            &keys,
            host,
            method_of(method),
            &request,
            Timestamp::from_secs(created_at),
        )
        .unwrap()
    }

    #[test]
    fn relay_host_url_maps_websocket_to_http_scheme() {
        let wss = RelayUrl::parse("wss://relay.example.com/prefix").unwrap();
        let ws = RelayUrl::parse("ws://127.0.0.1:7000").unwrap();
        assert_eq!(
            relay_host_url(&wss).unwrap().as_str(),
            "https://relay.example.com/prefix"
        );
        assert_eq!(relay_host_url(&ws).unwrap().scheme(), "http");
    }

    #[test]
    fn push_contract_wallet_event_matches_relay_fixture() {
        let fixture = fixture();
        for request in &fixture.requests {
            let signed = sign(&fixture, &fixture.host_url, &request.method, fixture.now);
            assert_eq!(signed.body, request.body);
            let token = signed.authorization.strip_prefix("Nostr ").unwrap();
            let event: Event =
                serde_json::from_slice(&BASE64_URL_SAFE_NO_PAD.decode(token).unwrap()).unwrap();
            assert_eq!(event.id, request.event.id);
            assert_eq!(event.pubkey, fixture.pubkey);
            assert_eq!(event.tags, request.event.tags);
            assert_eq!(event.created_at, request.event.created_at);
            assert_eq!(event.content, request.event.content);
        }
    }

    #[test]
    fn push_contract_fixture_passes_relay_checks() {
        let fixture = fixture();
        for request in &fixture.requests {
            assert_eq!(request.url, push_url(&fixture.host_url).unwrap());
            let key = relay_checks(
                &request.authorization,
                &request.url,
                &request.method,
                &request.body,
                fixture.now,
            );
            assert_eq!(key, Ok(fixture.pubkey));
        }
    }

    #[test]
    fn push_contract_wallet_event_passes_relay_checks() {
        let fixture = fixture();
        let url = push_url(&fixture.host_url).unwrap();
        for request in &fixture.requests {
            let signed = sign(&fixture, &fixture.host_url, &request.method, fixture.now);
            let key = relay_checks(
                &signed.authorization,
                &url,
                &request.method,
                &signed.body,
                fixture.now,
            );
            assert_eq!(key, Ok(fixture.pubkey));
        }
    }

    #[test]
    fn push_contract_event_window_is_sixty_seconds() {
        let fixture = fixture();
        let url = push_url(&fixture.host_url).unwrap();
        for (created_at, accepted) in [
            (fixture.now + 60, true),
            (fixture.now + 61, false),
            (fixture.now - 60, true),
            (fixture.now - 61, false),
        ] {
            let signed = sign(&fixture, &fixture.host_url, "PUT", created_at);
            let result = relay_checks(
                &signed.authorization,
                &url,
                "PUT",
                &signed.body,
                fixture.now,
            );
            if accepted {
                assert_eq!(result, Ok(fixture.pubkey), "created_at {created_at}");
            } else {
                assert_eq!(
                    result,
                    Err(Rejection::TimeWindow),
                    "created_at {created_at}"
                );
            }
        }
    }

    #[test]
    fn push_contract_event_for_other_host_is_rejected() {
        let fixture = fixture();
        let url = push_url(&fixture.host_url).unwrap();
        let other = Url::parse("https://relay.other.example.com").unwrap();
        let signed = sign(&fixture, &other, "PUT", fixture.now);
        let result = relay_checks(
            &signed.authorization,
            &url,
            "PUT",
            &signed.body,
            fixture.now,
        );
        assert_eq!(result, Err(Rejection::Url));
    }

    #[test]
    fn push_contract_event_is_bound_to_method_and_body() {
        let fixture = fixture();
        let url = push_url(&fixture.host_url).unwrap();
        let signed = sign(&fixture, &fixture.host_url, "PUT", fixture.now);
        assert_eq!(
            relay_checks(
                &signed.authorization,
                &url,
                "DELETE",
                &signed.body,
                fixture.now
            ),
            Err(Rejection::Method)
        );
        assert_eq!(
            relay_checks(&signed.authorization, &url, "PUT", "{}", fixture.now),
            Err(Rejection::Payload)
        );
    }

    #[test]
    fn push_contract_payload_round_trips_with_relay_field_names() {
        let json = format!(
            r#"{{"event_id":"{}","relay_url":"wss://relay.example.com","kind":1059}}"#,
            "ab".repeat(32)
        );
        let payload: PushPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload.kind, 1059);
        assert_eq!(serde_json::to_string(&payload).unwrap(), json);
    }

    #[test]
    fn push_contract_payload_accepts_string_kind_of_fcm_data() {
        let json = format!(
            r#"{{"event_id":"{}","relay_url":"wss://relay.example.com","kind":"1059"}}"#,
            "ab".repeat(32)
        );
        assert_eq!(
            serde_json::from_str::<PushPayload>(&json).unwrap().kind,
            1059
        );
    }

    #[test]
    fn push_contract_url_drops_every_trailing_slash() {
        for host in ["https://h//", "https://h///", "https://h/a//"] {
            let url = push_url(&Url::parse(host).unwrap()).unwrap();
            assert!(!url.path().contains("//"), "{url}");
        }
    }

    #[test]
    fn push_contract_url_keeps_host_path() {
        let host = Url::parse("https://relay.example.com/a").unwrap();
        let url = push_url(&host).unwrap();
        assert_eq!(url.as_str(), "https://relay.example.com/a/push");
        let slashed = Url::parse("https://relay.example.com/a/").unwrap();
        assert_eq!(push_url(&slashed).unwrap(), url);
        let root = Url::parse("https://relay.example.com").unwrap();
        assert_eq!(
            push_url(&root).unwrap().as_str(),
            "https://relay.example.com/push"
        );
    }
}
