//! Implement forever-caching for immutable paths.
//! See: https://docs.rs/axum/latest/axum/middleware/fn.from_fn.html

use axum::{extract::Request, http::{header::{ETAG, IF_NONE_MATCH}, HeaderMap, StatusCode}, middleware::Next, response::{IntoResponse, Response}};

/// note: Only caches forever on 200 responses.
pub async fn cache_forever(
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if headers.contains_key(IF_NONE_MATCH) {
        // We don't even care what etag you have. This thing is immutable.
        let response = (axum::http::status::StatusCode::NOT_MODIFIED).into_response();
        return Ok(response);
    }

    let mut response = next.run(request).await;
    if response.status().is_success() {
        response.headers_mut().insert(ETAG, "\"immutable\"".try_into().expect("etag"));
    }
    return Ok(response);

}