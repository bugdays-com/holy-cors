use http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode};
use http_body_util::Full;
use bytes::Bytes;

use crate::config::Config;

/// CORS headers to add to responses
const CORS_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS";
const CORS_MAX_AGE: &str = "86400";

/// Check if the request origin is allowed
pub fn check_origin(headers: &HeaderMap, config: &Config) -> Result<String, Response<Full<Bytes>>> {
    // Get the Origin header
    let origin = match headers.get(header::ORIGIN) {
        Some(origin) => match origin.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "Invalid Origin header",
                ));
            }
        },
        None => {
            // No Origin header - this could be a direct request (curl, etc.)
            // Allow it but return empty string for origin
            return Ok(String::new());
        }
    };

    // Check if origin is allowed
    if !config.is_origin_allowed(&origin) {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            &format!("Origin '{}' is not allowed. Use --allow-origin to add it.", origin),
        ));
    }

    Ok(origin)
}

/// Handle preflight OPTIONS request
pub fn handle_preflight(origin: &str, request_headers: &HeaderMap) -> Response<Full<Bytes>> {
    let mut response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Full::new(Bytes::new()))
        .unwrap();

    add_cors_headers(response.headers_mut(), origin, request_headers);
    response
}

/// Add CORS headers to a response
pub fn add_cors_headers(headers: &mut HeaderMap, origin: &str, request_headers: &HeaderMap) {
    // Access-Control-Allow-Origin
    if !origin.is_empty() {
        if let Ok(value) = HeaderValue::from_str(origin) {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        }
    } else {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    }

    // Access-Control-Allow-Methods
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(CORS_METHODS),
    );

    // Access-Control-Allow-Headers - echo back requested headers or allow all
    if let Some(requested_headers) = request_headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested_headers.clone());
    } else {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("*"),
        );
    }

    // Access-Control-Expose-Headers - expose all headers
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("*"),
    );

    // Access-Control-Max-Age
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(CORS_MAX_AGE),
    );

    // Access-Control-Allow-Credentials
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
}

/// Check if the request is a preflight OPTIONS request
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Create an error response with CORS headers
pub fn error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let body = format!(r#"{{"error": "{}"}}"#, message);

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, CORS_METHODS)
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

/// Create a success response with a message
pub fn success_response(message: &str) -> Response<Full<Bytes>> {
    let body = format!(r#"{{"message": "{}"}}"#, message);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}
