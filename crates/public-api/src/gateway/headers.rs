//! The headers that Edge sends to a domain service and relays back from it.
//!
//! Edge mints the identity claims onto a forwarded request in the reserved `x-ratatoskr-`
//! namespace, after it removes every reserved header, credential and hop-by-hop field that the
//! client sent. On the way back it removes the hop-by-hop fields and every reserved header that a
//! domain service tried to mint.

use std::collections::HashSet;

use http::header::{AUTHORIZATION, CONNECTION, COOKIE, HOST};
use http::{HeaderMap, HeaderName, HeaderValue};
use ratatoskr_ai_archive_contracts::platform_receipt;

use super::Claims;

const RESERVED_PREFIX: &str = "x-ratatoskr-";

/// The claim that names the stored kind of the authenticated device (`obsidian_plugin`,
/// `export_agent`, ...). `ratatoskr-contracts` names no constant for it yet. It carries the
/// reserved prefix, so a copy that a client sends is removed with every other reserved header.
const HEADER_DEVICE_KIND: &str = "x-ratatoskr-device-kind";

/// The claim that names the kind of the authenticated session (`browser`, `device`, ...), so that
/// notes tells the owner's upload from a device upload. Reserved prefix, as above.
const HEADER_SESSION_KIND: &str = "x-ratatoskr-session-kind";

/// The value of [`HEADER_SESSION_KIND`] on a request of the `public` class (ADR-0017).
const ANONYMOUS_SESSION_KIND: &str = "anonymous";

pub(super) fn forwarded_headers(headers: &HeaderMap, claims: Claims<'_>) -> HeaderMap {
    let Claims {
        principal,
        device_kind,
        correlation_id,
    } = claims;
    let mut forwarded = cleaned(headers);
    let user_id = principal.user_id.to_string();
    insert(&mut forwarded, platform_receipt::HEADER_USER_ID, &user_id);
    if let Some(device_id) = principal.device_id {
        let device_id = device_id.to_string();
        insert(
            &mut forwarded,
            platform_receipt::HEADER_DEVICE_ID,
            &device_id,
        );
    }
    if let Some(device_kind) = device_kind {
        insert(&mut forwarded, HEADER_DEVICE_KIND, device_kind);
    }
    insert(&mut forwarded, HEADER_SESSION_KIND, principal.kind.as_str());
    insert(
        &mut forwarded,
        platform_receipt::HEADER_CORRELATION_ID,
        correlation_id,
    );
    forwarded
}

/// The headers of a request of the `public` class (ADR-0017): the same hygiene as
/// [`forwarded_headers`], so no credential, cookie or reserved header of the client passes, and the
/// anonymous claim, which is the session kind `anonymous` and the correlation id. It holds no user
/// and no device.
pub(super) fn anonymous_headers(headers: &HeaderMap, correlation_id: &str) -> HeaderMap {
    let mut forwarded = cleaned(headers);
    insert(&mut forwarded, HEADER_SESSION_KIND, ANONYMOUS_SESSION_KIND);
    insert(
        &mut forwarded,
        platform_receipt::HEADER_CORRELATION_ID,
        correlation_id,
    );
    forwarded
}

/// The headers of a client request without its credentials, its cookies, its reserved headers and
/// its hop-by-hop fields.
fn cleaned(headers: &HeaderMap) -> HeaderMap {
    let connection_tokens = connection_tokens(headers);
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if name.as_str().starts_with(RESERVED_PREFIX)
            || connection_tokens.contains(name)
            || matches!(
                name,
                &CONNECTION
                    | &AUTHORIZATION
                    | &COOKIE
                    | &HOST
                    | &http::header::PROXY_AUTHENTICATE
                    | &http::header::PROXY_AUTHORIZATION
                    | &http::header::TE
                    | &http::header::TRAILER
                    | &http::header::TRANSFER_ENCODING
                    | &http::header::UPGRADE
            )
            || name == "keep-alive"
        {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }
    forwarded
}

pub(super) fn archive_headers(
    mut headers: HeaderMap,
    operation_id: uuid::Uuid,
    sha256: &str,
    byte_size: u64,
) -> HeaderMap {
    insert(
        &mut headers,
        platform_receipt::HEADER_OPERATION_ID,
        &operation_id.to_string(),
    );
    insert(
        &mut headers,
        platform_receipt::HEADER_ARCHIVE_SHA256,
        sha256,
    );
    insert(
        &mut headers,
        platform_receipt::HEADER_ARCHIVE_BYTE_SIZE,
        &byte_size.to_string(),
    );
    headers
}

/// Remove hop-by-hop fields from a downstream response and prevent a domain service from minting
/// a header in Edge's reserved namespace. `Connection` can nominate arbitrary header names, so it
/// is parsed rather than treated as one fixed field.
pub(super) fn response_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_tokens = connection_tokens(headers);
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if name.as_str().starts_with(RESERVED_PREFIX)
            || connection_tokens.contains(name)
            || matches!(
                name,
                &CONNECTION
                    | &http::header::PROXY_AUTHENTICATE
                    | &http::header::PROXY_AUTHORIZATION
                    | &http::header::TE
                    | &http::header::TRAILER
                    | &http::header::TRANSFER_ENCODING
                    | &http::header::UPGRADE
            )
            || name == "keep-alive"
        {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }
    forwarded
}

fn connection_tokens(headers: &HeaderMap) -> HashSet<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| HeaderName::from_bytes(value.trim().as_bytes()).ok())
        .collect()
}

pub(super) fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}
