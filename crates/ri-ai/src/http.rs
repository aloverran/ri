use std::pin::Pin;
use futures::Stream;

use ri::ApiError;

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>;

pub async fn send(builder: reqwest::RequestBuilder) -> Result<ByteStream, ApiError> {
    let response = builder.send().await.map_err(|e| ApiError::Http(e.to_string()))?;
    let status = response.status().as_u16();

    if status >= 400 {
        let body = response.text().await.unwrap_or_default();
        return Err(parse_http_error(status, &body));
    }

    Ok(Box::pin(response.bytes_stream()))
}

fn parse_http_error(status: u16, body: &str) -> ApiError {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();

    if let Some(error) = parsed.get("error") {
        let error_type = error["type"].as_str().unwrap_or("unknown");
        let message = error["message"].as_str().unwrap_or(body).to_string();

        if error_type == "rate_limit_error" || status == 429 {
            return ApiError::RateLimited { retry_after_ms: 5000 };
        }
        if message.contains("token") && (message.contains("exceed") || message.contains("limit")) {
            return ApiError::ContextOverflow { used: 0, limit: 0 };
        }
        return ApiError::Api { status, message: format!("{}: {}", error_type, message) };
    }

    if status == 429 || body.contains("RESOURCE_EXHAUSTED") {
        return ApiError::RateLimited { retry_after_ms: 5000 };
    }

    ApiError::Api { status, message: body.to_string() }
}
