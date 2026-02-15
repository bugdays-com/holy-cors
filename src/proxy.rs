use bytes::Bytes;
use http::{header, HeaderMap, Request, Response, StatusCode, Uri};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::sync::Arc;
use tokio_tungstenite::connect_async;
use tracing::{debug, error, info, warn};
use url::Url;

use crate::config::Config;
use crate::cors::{add_cors_headers, check_origin, error_response, handle_preflight, is_preflight, success_response};

/// Headers that should not be forwarded to the target
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

/// Headers that should not be forwarded back to the client
const SKIP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-encoding",
    "content-length",
];

/// Main proxy request handler
pub async fn handle_request(
    req: Request<Incoming>,
    config: Arc<Config>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let uri = req.uri().clone();

    debug!("Received request: {} {}", method, uri);

    // Check origin
    let origin = match check_origin(&headers, &config) {
        Ok(origin) => origin,
        Err(response) => return Ok(response.map(|b| b.map_err(|_| unreachable!()).boxed())),
    };

    // Handle preflight
    if is_preflight(&method, &headers) {
        debug!("Handling preflight request");
        return Ok(handle_preflight(&origin, &headers).map(|b| b.map_err(|_| unreachable!()).boxed()));
    }

    // Handle root path - return welcome message
    let path = uri.path();
    if path == "/" || path.is_empty() {
        return Ok(success_response("Holy CORS! Proxy is running. Usage: /{TARGET_URL}")
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
    }

    // Extract target URL from path (everything after the first /)
    let target_url = extract_target_url(&uri);
    let target_url = match target_url {
        Some(url) => url,
        None => {
            return Ok(error_response(StatusCode::BAD_REQUEST, "Invalid target URL")
                .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    // Parse and validate the target URL
    let parsed_url = match Url::parse(&target_url) {
        Ok(url) => url,
        Err(e) => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid URL: {}", e),
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    // Validate scheme
    match parsed_url.scheme() {
        "http" | "https" => {}
        scheme => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                &format!("Unsupported scheme: {}. Only http and https are allowed.", scheme),
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    }

    info!("Proxying {} {} -> {}", method, uri, target_url);

    // Check for WebSocket upgrade
    if is_websocket_upgrade(&headers) {
        return handle_websocket(&target_url).await;
    }

    // Forward the request
    forward_request(req, &target_url, &origin).await
}

/// Extract the target URL from the request path
fn extract_target_url(uri: &Uri) -> Option<String> {
    let path = uri.path();

    // Remove the leading slash
    let path = path.strip_prefix('/').unwrap_or(path);

    if path.is_empty() {
        return None;
    }

    // The path should be the full URL (possibly URL-encoded)
    // Handle both cases: /https://example.com and /https%3A%2F%2Fexample.com
    let decoded = urlencoding_decode(path);

    // Add query string if present
    let url = if let Some(query) = uri.query() {
        format!("{}?{}", decoded, query)
    } else {
        decoded
    };

    // Validate it looks like a URL
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url)
    } else {
        // Try adding https:// if it looks like a domain
        if url.contains('.') && !url.contains(' ') {
            Some(format!("https://{}", url))
        } else {
            None
        }
    }
}

/// Simple URL decoding
fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    result.push(byte as char);
                    continue;
                }
            }
            result.push('%');
            result.push_str(&hex);
        } else {
            result.push(c);
        }
    }

    result
}

/// Check if this is a WebSocket upgrade request
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

/// Forward an HTTP request to the target
async fn forward_request(
    req: Request<Incoming>,
    target_url: &str,
    origin: &str,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let method = req.method().clone();
    let original_headers = req.headers().clone();

    // Build HTTPS connector with HTTP/2 support using native roots
    let https = HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("Failed to load native TLS roots")
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();

    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .http2_only(false)
        .build(https);

    // Parse target URI
    let target_uri: Uri = match target_url.parse() {
        Ok(uri) => uri,
        Err(e) => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid target URI: {}", e),
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    // Collect the request body
    let body_bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return Ok(error_response(StatusCode::BAD_REQUEST, "Failed to read request body")
                .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    // Build the proxied request
    let mut builder = Request::builder()
        .method(method)
        .uri(&target_uri);

    // Forward headers (excluding hop-by-hop headers)
    for (name, value) in original_headers.iter() {
        let name_str = name.as_str().to_lowercase();
        if !HOP_BY_HOP_HEADERS.contains(&name_str.as_str()) {
            builder = builder.header(name, value);
        }
    }

    // Set the Host header to the target
    if let Some(host) = target_uri.host() {
        let host_value = if let Some(port) = target_uri.port() {
            format!("{}:{}", host, port)
        } else {
            host.to_string()
        };
        builder = builder.header(header::HOST, host_value);
    }

    let proxy_req = match builder.body(Full::new(body_bytes)) {
        Ok(req) => req,
        Err(e) => {
            error!("Failed to build proxy request: {}", e);
            return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to build request")
                .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    // Send the request
    let response: Response<Incoming> = match client.request(proxy_req).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Proxy request failed: {}", e);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to reach target: {}", e),
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    // Build the response with CORS headers
    let (mut parts, body) = response.into_parts();

    // Remove headers we don't want to forward back
    for header_name in SKIP_RESPONSE_HEADERS {
        if let Ok(name) = header::HeaderName::from_bytes(header_name.as_bytes()) {
            parts.headers.remove(&name);
        }
    }

    // Add CORS headers
    add_cors_headers(&mut parts.headers, origin, &original_headers);

    // Convert the response body to BoxBody
    let boxed_body: BoxBody<Bytes, hyper::Error> = body.boxed();

    Ok(Response::from_parts(parts, boxed_body))
}

/// Handle WebSocket upgrade and proxy
async fn handle_websocket(
    target_url: &str,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    info!("WebSocket upgrade requested for {}", target_url);

    // Convert http:// to ws:// and https:// to wss://
    let ws_url = target_url
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);

    // For now, we return an error indicating WebSocket support is limited
    // Full WebSocket proxying requires a different approach with connection hijacking
    // which isn't directly supported by hyper 1.0 without additional work

    warn!("WebSocket proxying is experimental");

    // Try to connect to the target WebSocket
    let (_ws_stream, _) = match connect_async(&ws_url).await {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to connect to WebSocket target: {}", e);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to connect to WebSocket: {}", e),
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    // For full WebSocket proxying, we'd need to upgrade the incoming connection
    // and bidirectionally proxy messages. This requires connection hijacking.
    // For now, return an informational response.

    Ok(error_response(
        StatusCode::NOT_IMPLEMENTED,
        "WebSocket proxying requires connection hijacking. Use a direct WebSocket connection.",
    )
    .map(|b| b.map_err(|_| unreachable!()).boxed()))
}
