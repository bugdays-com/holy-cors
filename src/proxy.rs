use bytes::Bytes;
use http::{header, HeaderMap, Method, Request, Response, StatusCode, Uri};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::{debug, error, info, warn};
use url::Url;

use crate::config::Config;
use crate::cors::{
    add_cors_headers, check_origin, error_response, handle_preflight, is_preflight,
    success_response,
};
use crate::dns::{self, DnsQueryRequest};
use crate::kafka;
use crate::tls_inspector::{self, TlsInspectRequest};

const CAPABILITIES_PATH: &str = "/api/v1/capabilities";
const DNS_QUERY_PATH: &str = "/api/v1/dns/query";
const TLS_INSPECT_PATH: &str = "/api/v1/tls/inspect";
const MAX_API_BODY_BYTES: usize = 32 * 1024;
const MAX_KAFKA_BODY_BYTES: usize = 24 * 1024 * 1024;
const BRIDGE_MODE_HEADER: &str = "x-holy-cors-mode";
const NATIVE_GRPC_MODE: &str = "grpc-native";

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
        Err(response) => return Ok((*response).map(|b| b.map_err(|_| unreachable!()).boxed())),
    };

    // Handle preflight
    if is_preflight(&method, &headers) {
        debug!("Handling preflight request");
        return Ok(
            handle_preflight(&origin, &headers).map(|b| b.map_err(|_| unreachable!()).boxed())
        );
    }

    // Handle root path - return welcome message
    let path = uri.path();
    if path == CAPABILITIES_PATH {
        return Ok(
            capabilities_response(&origin, &headers).map(|b| b.map_err(|_| unreachable!()).boxed())
        );
    }

    if path == DNS_QUERY_PATH {
        return Ok(handle_dns_query(req, &origin, &headers).await);
    }

    if path == TLS_INSPECT_PATH {
        return Ok(handle_tls_inspect(req, &origin, &headers).await);
    }

    if path.starts_with("/api/v1/kafka/") {
        return Ok(handle_kafka(req, path.to_string(), &origin, &headers).await);
    }

    if path == "/" || path.is_empty() {
        return Ok(success_response(
            "Holy CORS is running — the Bug Days local API bridge is ready. Usage: /{TARGET_URL}",
        )
        .map(|b| b.map_err(|_| unreachable!()).boxed()));
    }

    // Extract target URL from path (everything after the first /)
    let target_url = extract_target_url(&uri);
    let target_url = match target_url {
        Some(url) => url,
        None => {
            return Ok(
                error_response(StatusCode::BAD_REQUEST, "Invalid target URL")
                    .map(|b| b.map_err(|_| unreachable!()).boxed()),
            );
        }
    };

    // Parse and validate the target URL
    let parsed_url = match Url::parse(&target_url) {
        Ok(url) => url,
        Err(e) => {
            return Ok(
                error_response(StatusCode::BAD_REQUEST, &format!("Invalid URL: {}", e))
                    .map(|b| b.map_err(|_| unreachable!()).boxed()),
            );
        }
    };

    // Validate scheme
    match parsed_url.scheme() {
        "http" | "https" => {}
        scheme => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                &format!(
                    "Unsupported scheme: {}. Only http and https are allowed.",
                    scheme
                ),
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    }

    info!("Proxying {} {} -> {}", method, uri, target_url);

    // Check for WebSocket upgrade
    if is_websocket_upgrade(&headers) {
        return handle_websocket(&target_url).await;
    }

    // Native gRPC needs protocol translation in addition to an HTTP/2-capable
    // upstream connection. The browser opts into this path explicitly so an
    // existing gRPC-Web endpoint can still be proxied without modification.
    if is_native_grpc_request(&headers) {
        forward_native_grpc(req, &target_url, &origin).await
    } else {
        forward_request(req, &target_url, &origin).await
    }
}

fn capabilities_response(origin: &str, request_headers: &HeaderMap) -> Response<Full<Bytes>> {
    let body = format!(
        r#"{{"name":"Holy CORS","product":"Bug Days Local Bridge","version":"{}","protocolVersion":2,"capabilities":{{"httpProxy":true,"grpcWebProxy":true,"grpcNativeBridge":true,"unaryGrpc":true,"serverStreamingGrpc":true,"clientStreamingGrpc":false,"bidirectionalStreamingGrpc":false,"webSocketTunneling":false,"dnsLookup":true,"reverseDns":true,"tlsInspection":true,"rawTls":true,"kafkaApiVersion":1}}}}"#,
        env!("CARGO_PKG_VERSION")
    );

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(body)))
        .expect("valid capabilities response");
    add_cors_headers(response.headers_mut(), origin, request_headers);
    response
}

async fn handle_dns_query(
    req: Request<Incoming>,
    origin: &str,
    request_headers: &HeaderMap,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    if req.method() != Method::POST {
        return api_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Use POST for DNS queries.",
            origin,
            request_headers,
        );
    }
    let request = match parse_json_body::<DnsQueryRequest>(req).await {
        Ok(value) => value,
        Err((status, message)) => return api_error(status, &message, origin, request_headers),
    };
    match dns::query(request).await {
        Ok(response) => api_json(StatusCode::OK, &response, origin, request_headers),
        Err(message) => api_error(StatusCode::BAD_REQUEST, &message, origin, request_headers),
    }
}

async fn handle_tls_inspect(
    req: Request<Incoming>,
    origin: &str,
    request_headers: &HeaderMap,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    if req.method() != Method::POST {
        return api_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Use POST for TLS inspection.",
            origin,
            request_headers,
        );
    }
    let request = match parse_json_body::<TlsInspectRequest>(req).await {
        Ok(value) => value,
        Err((status, message)) => return api_error(status, &message, origin, request_headers),
    };
    match tls_inspector::inspect(request).await {
        Ok(response) => api_json(StatusCode::OK, &response, origin, request_headers),
        Err(message) => api_error(StatusCode::BAD_GATEWAY, &message, origin, request_headers),
    }
}

async fn parse_json_body<T: DeserializeOwned>(
    req: Request<Incoming>,
) -> Result<T, (StatusCode, String)> {
    parse_json_body_limit(req, MAX_API_BODY_BYTES).await
}

async fn parse_json_body_limit<T: DeserializeOwned>(
    req: Request<Incoming>,
    max_bytes: usize,
) -> Result<T, (StatusCode, String)> {
    if req
        .body()
        .size_hint()
        .upper()
        .is_some_and(|size| size as usize > max_bytes)
    {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body is too large.".to_string(),
        ));
    }
    let bytes = req
        .into_body()
        .collect()
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Could not read the request body.".to_string(),
            )
        })?
        .to_bytes();
    if bytes.len() > max_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body is too large.".to_string(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid JSON request: {error}"),
        )
    })
}

async fn handle_kafka(
    req: Request<Incoming>,
    path: String,
    origin: &str,
    request_headers: &HeaderMap,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    if req.method() != Method::POST {
        return api_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Use POST for Kafka requests.",
            origin,
            request_headers,
        );
    }
    let body = match parse_json_body_limit::<serde_json::Value>(req, MAX_KAFKA_BODY_BYTES).await {
        Ok(body) => body,
        Err((status, message)) => return api_error(status, &message, origin, request_headers),
    };
    match kafka::execute(&path, body, origin).await {
        Ok(value) => api_json(StatusCode::OK, &value, origin, request_headers),
        Err(error) => api_error(error.status, &error.message, origin, request_headers),
    }
}

fn api_error(
    status: StatusCode,
    message: &str,
    origin: &str,
    request_headers: &HeaderMap,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    #[derive(Serialize)]
    struct ErrorBody<'a> {
        error: &'a str,
    }
    api_json(
        status,
        &ErrorBody { error: message },
        origin,
        request_headers,
    )
}

fn api_json<T: Serialize>(
    status: StatusCode,
    payload: &T,
    origin: &str,
    request_headers: &HeaderMap,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    let body = serde_json::to_vec(payload)
        .unwrap_or_else(|_| b"{\"error\":\"Could not encode response.\"}".to_vec());
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(body)))
        .expect("valid API response");
    add_cors_headers(response.headers_mut(), origin, request_headers);
    response.map(|body| body.map_err(|_| unreachable!()).boxed())
}

fn is_native_grpc_request(headers: &HeaderMap) -> bool {
    headers
        .get(BRIDGE_MODE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case(NATIVE_GRPC_MODE))
        .unwrap_or(false)
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
            return Ok(
                error_response(StatusCode::BAD_REQUEST, "Failed to read request body")
                    .map(|b| b.map_err(|_| unreachable!()).boxed()),
            );
        }
    };

    // Build the proxied request
    let mut builder = Request::builder().method(method).uri(&target_uri);

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
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build request",
            )
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

/// Translate a binary gRPC-Web request from the browser into native gRPC over
/// HTTP/2. gRPC-Web data messages deliberately use the native five-byte gRPC
/// frame, so request data can be forwarded byte-for-byte. Response trailers,
/// which browser APIs cannot observe, are converted into the final gRPC-Web
/// trailer frame by `GrpcWebResponseBody`.
async fn forward_native_grpc(
    req: Request<Incoming>,
    target_url: &str,
    origin: &str,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let method = req.method().clone();
    let original_headers = req.headers().clone();

    if method != http::Method::POST {
        return Ok(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Native gRPC requests must use POST",
        )
        .map(|b| b.map_err(|_| unreachable!()).boxed()));
    }

    let content_type = original_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type.starts_with("application/grpc-web") {
        return Ok(error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Native gRPC bridge expects a binary application/grpc-web request",
        )
        .map(|b| b.map_err(|_| unreachable!()).boxed()));
    }
    if content_type.starts_with("application/grpc-web-text") {
        return Ok(error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Text-mode gRPC-Web is not supported by the native bridge; use binary protobuf mode",
        )
        .map(|b| b.map_err(|_| unreachable!()).boxed()));
    }

    let target_uri: Uri = match target_url.parse() {
        Ok(uri) => uri,
        Err(e) => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid native gRPC target: {}", e),
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    let body_bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to read gRPC-Web request body: {}", e);
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "Failed to read gRPC-Web request body",
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    if body_bytes.len() < 5 {
        return Ok(
            error_response(StatusCode::BAD_REQUEST, "Invalid gRPC-Web frame")
                .map(|b| b.map_err(|_| unreachable!()).boxed()),
        );
    }

    let native_content_type = content_type.replacen("application/grpc-web", "application/grpc", 1);
    let mut builder = Request::builder()
        .method(method)
        .uri(&target_uri)
        .header(header::CONTENT_TYPE, native_content_type)
        .header("te", "trailers")
        .header(
            header::USER_AGENT,
            concat!("holy-cors/", env!("CARGO_PKG_VERSION")),
        );

    for (name, value) in original_headers.iter() {
        let name_str = name.as_str().to_ascii_lowercase();
        let browser_only = name_str == BRIDGE_MODE_HEADER
            || name_str == "x-grpc-web"
            || name_str == "x-user-agent"
            || name_str == "origin"
            || name_str == "referer"
            || name_str.starts_with("sec-fetch-")
            || name == header::CONTENT_TYPE
            || name == header::CONTENT_LENGTH
            || name == header::USER_AGENT;
        if !browser_only && !HOP_BY_HOP_HEADERS.contains(&name_str.as_str()) {
            builder = builder.header(name, value);
        }
    }

    if let Some(host) = target_uri.host() {
        let host_value = target_uri
            .port()
            .map(|port| format!("{}:{}", host, port))
            .unwrap_or_else(|| host.to_string());
        builder = builder.header(header::HOST, host_value);
    }

    let proxy_req = match builder.body(Full::new(body_bytes)) {
        Ok(req) => req,
        Err(e) => {
            error!("Failed to build native gRPC request: {}", e);
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build native gRPC request",
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    // Native gRPC requires HTTP/2. For HTTPS this uses ALPN; for plaintext
    // endpoints Hyper uses HTTP/2 prior knowledge (h2c), matching grpcurl's
    // common -plaintext development setup.
    let https = HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("Failed to load native TLS roots")
        .https_or_http()
        .enable_http2()
        .build();
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build(https);

    let response: Response<Incoming> = match client.request(proxy_req).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Native gRPC request failed: {}", e);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to reach native gRPC target over HTTP/2: {}", e),
            )
            .map(|b| b.map_err(|_| unreachable!()).boxed()));
        }
    };

    let (mut parts, body) = response.into_parts();
    for header_name in SKIP_RESPONSE_HEADERS {
        if let Ok(name) = header::HeaderName::from_bytes(header_name.as_bytes()) {
            parts.headers.remove(&name);
        }
    }
    let is_grpc_response = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/grpc"))
        .unwrap_or(false);
    if is_grpc_response {
        parts.headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/grpc-web+proto"),
        );
    }
    parts.headers.insert(
        "x-holy-cors-mode",
        header::HeaderValue::from_static(NATIVE_GRPC_MODE),
    );
    parts.headers.insert(
        "x-holy-cors-version",
        header::HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );
    add_cors_headers(&mut parts.headers, origin, &original_headers);

    let boxed_body: BoxBody<Bytes, hyper::Error> = if is_grpc_response {
        GrpcWebResponseBody::new(body).boxed()
    } else {
        body.boxed()
    };
    Ok(Response::from_parts(parts, boxed_body))
}

struct GrpcWebResponseBody {
    inner: Incoming,
}

impl GrpcWebResponseBody {
    fn new(inner: Incoming) -> Self {
        Self { inner }
    }
}

impl Body for GrpcWebResponseBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                Ok(data) => Poll::Ready(Some(Ok(Frame::data(data)))),
                Err(frame) => match frame.into_trailers() {
                    Ok(trailers) => {
                        Poll::Ready(Some(Ok(Frame::data(encode_grpc_web_trailers(&trailers)))))
                    }
                    Err(_) => Poll::Ready(None),
                },
            },
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        // The native trailer block becomes response data, so the exact final
        // length is not known until the upstream stream completes.
        SizeHint::new()
    }
}

fn encode_grpc_web_trailers(trailers: &HeaderMap) -> Bytes {
    let mut block = Vec::new();
    for (name, value) in trailers.iter() {
        block.extend_from_slice(name.as_str().to_ascii_lowercase().as_bytes());
        block.extend_from_slice(b": ");
        block.extend_from_slice(value.as_bytes());
        block.extend_from_slice(b"\r\n");
    }

    let mut frame = Vec::with_capacity(5 + block.len());
    frame.push(0x80);
    frame.extend_from_slice(&(block.len() as u32).to_be_bytes());
    frame.extend_from_slice(&block);
    Bytes::from(frame)
}

/// Handle WebSocket upgrade and proxy
async fn handle_websocket(
    target_url: &str,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    info!("WebSocket upgrade requested for {}", target_url);

    warn!("WebSocket tunneling is not implemented");

    Ok(error_response(
        StatusCode::NOT_IMPLEMENTED,
        "WebSocket tunneling is not supported yet. Use a direct WebSocket connection.",
    )
    .map(|b| b.map_err(|_| unreachable!()).boxed()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_explicit_native_grpc_mode() {
        let mut headers = HeaderMap::new();
        assert!(!is_native_grpc_request(&headers));
        headers.insert(
            BRIDGE_MODE_HEADER,
            header::HeaderValue::from_static("grpc-native"),
        );
        assert!(is_native_grpc_request(&headers));
    }

    #[test]
    fn encodes_native_trailers_as_grpc_web_frame() {
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", header::HeaderValue::from_static("0"));
        trailers.insert("grpc-message", header::HeaderValue::from_static("all-good"));

        let encoded = encode_grpc_web_trailers(&trailers);
        assert_eq!(encoded[0], 0x80);
        let length = u32::from_be_bytes(encoded[1..5].try_into().unwrap()) as usize;
        assert_eq!(length, encoded.len() - 5);

        let text = std::str::from_utf8(&encoded[5..]).unwrap();
        assert!(text.contains("grpc-status: 0\r\n"));
        assert!(text.contains("grpc-message: all-good\r\n"));
    }
}
