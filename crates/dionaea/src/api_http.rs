//! The HTTP side of the API: turns a request into an [`ApiRequest`], runs the
//! handler on a blocking thread and writes the response.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, COOKIE, HOST, ORIGIN, RETRY_AFTER, SET_COOKIE,
};
use axum::http::{HeaderValue, Request, Response, StatusCode};
use axum::routing::any;
use tracing::error;

use crate::api::{Api, ApiRequest, ApiResponse};

/// The largest body: a schema file is at most 1 MiB, and everything else is a
/// few hundred bytes.
const MAX_BODY_BYTES: usize = (1 << 20) + 4096;
/// How long a client may take to send the body.
const BODY_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct HttpState {
    pub(crate) api: Arc<Api>,
    /// Reverse proxies in front of the server, and the header they append the
    /// client address to; as for the collect endpoint.
    pub(crate) trusted_proxy_hops: u8,
    pub(crate) client_address_header: String,
}

pub(crate) fn routes(state: Arc<HttpState>) -> Router {
    Router::new().fallback(any(handle)).with_state(state)
}

fn header(request: &Request<Body>, name: impl axum::http::header::AsHeaderName) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

async fn handle(
    State(state): State<Arc<HttpState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let client = client_key(
        request.headers(),
        peer.ip(),
        state.trusted_proxy_hops,
        &state.client_address_header,
    );
    let (parts, body) = request.into_parts();
    let request = Request::from_parts(parts, Body::empty());
    let mut api_request = ApiRequest {
        method: request.method().as_str().to_owned(),
        path: request.uri().path().to_owned(),
        query: request.uri().query().map(str::to_owned),
        cookie: header(&request, COOKIE),
        authorization: header(&request, AUTHORIZATION),
        origin: header(&request, ORIGIN),
        host: header(&request, HOST),
        content_type: header(&request, CONTENT_TYPE),
        body: Vec::new(),
        client,
    };
    match tokio::time::timeout(BODY_TIMEOUT, axum::body::to_bytes(body, MAX_BODY_BYTES)).await {
        Ok(Ok(bytes)) => api_request.body = bytes.to_vec(),
        Ok(Err(_)) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"),
        Err(_) => return plain(StatusCode::REQUEST_TIMEOUT, "request_timeout"),
    }
    let api = Arc::clone(&state.api);
    match tokio::task::spawn_blocking(move || api.respond(&api_request)).await {
        Ok(response) => write(response),
        Err(error) => {
            error!(%error, "an API handler failed");
            plain(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

/// What the login limiter counts a client as: the address, except that an
/// IPv6 client is its /64, which one host controls whole.
fn client_key(
    headers: &axum::http::HeaderMap,
    peer: std::net::IpAddr,
    hops: u8,
    header: &str,
) -> String {
    crate::collect::address_bucket(crate::collect::client_address(headers, peer, hops, header))
        .to_string()
}

fn plain(status: StatusCode, code: &str) -> Response<Body> {
    let body = format!("{{\"error\":{{\"code\":\"{code}\",\"message\":\"{code}\"}}}}");
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    harden(&mut response);
    response
}

fn harden(response: &mut Response<Body>) {
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
}

fn write(response: ApiResponse) -> Response<Body> {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = if status == StatusCode::NO_CONTENT {
        Body::empty()
    } else {
        Body::from(response.body.to_string())
    };
    let mut out = Response::new(body);
    *out.status_mut() = status;
    harden(&mut out);
    if let Some(cookie) = response
        .set_cookie
        .as_deref()
        .and_then(|c| HeaderValue::from_str(c).ok())
    {
        out.headers_mut().insert(SET_COOKIE, cookie);
    }
    if let Some(seconds) = response
        .retry_after
        .and_then(|s| HeaderValue::from_str(&s.max(1).to_string()).ok())
    {
        out.headers_mut().insert(RETRY_AFTER, seconds);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn an_ipv6_client_is_counted_by_its_prefix() {
        let headers = HeaderMap::new();
        let a: std::net::IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: std::net::IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let other: std::net::IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(
            client_key(&headers, a, 0, "x-forwarded-for"),
            client_key(&headers, b, 0, "x-forwarded-for")
        );
        assert_ne!(
            client_key(&headers, a, 0, "x-forwarded-for"),
            client_key(&headers, other, 0, "x-forwarded-for")
        );
        let v4: std::net::IpAddr = "192.0.2.7".parse().unwrap();
        assert_eq!(client_key(&headers, v4, 0, "x-forwarded-for"), "192.0.2.7");
    }
}
