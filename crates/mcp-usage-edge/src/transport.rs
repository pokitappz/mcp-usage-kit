//! Request validation and body deadlines before authentication and payment work.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Full};
use mcp_usage_kit::MeterBody;
use tower::{Layer, Service};

/// Invalid transport configuration, without echoing supplied values.
#[derive(Debug, thiserror::Error)]
#[error("transport limits must be positive and origins must be exact HTTP(S) origins")]
pub struct TransportConfigError;

fn valid_origin(value: &str) -> bool {
    let Ok(uri) = value.parse::<http::Uri>() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && uri.authority().is_some_and(|a| !a.as_str().contains('@'))
        && uri.query().is_none()
        && !value.contains('#')
        && value
            == format!(
                "{}://{}",
                uri.scheme_str().unwrap_or_default(),
                uri.authority().map_or("", |a| a.as_str())
            )
}

/// Configured request boundary for a sidecar or embedded transport.
#[derive(Debug, Clone)]
pub struct TransportLayer {
    origins: Arc<Vec<String>>,
    body_timeout: Duration,
}

impl TransportLayer {
    /// Construct an exact origin allowlist and a total request-body deadline.
    /// Requests without `Origin` are accepted.
    ///
    /// # Errors
    /// Rejects invalid origins and zero timeouts.
    pub fn new(origins: Vec<String>, body_timeout: Duration) -> Result<Self, TransportConfigError> {
        if body_timeout.is_zero() || origins.iter().any(|origin| !valid_origin(origin)) {
            return Err(TransportConfigError);
        }
        Ok(Self {
            origins: Arc::new(origins),
            body_timeout,
        })
    }

    fn validate(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let mut origins = headers.get_all(http::header::ORIGIN).iter();
        if let Some(origin) = origins.next()
            && (origins.next().is_some()
                || !origin
                    .to_str()
                    .is_ok_and(|o| self.origins.iter().any(|allowed| allowed == o)))
        {
            return Err(StatusCode::FORBIDDEN);
        }
        for connection in headers.get_all(http::header::CONNECTION) {
            let value = connection.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
            for name in value.split(',') {
                let name = http::header::HeaderName::from_bytes(name.trim().as_bytes())
                    .map_err(|_| StatusCode::BAD_REQUEST)?;
                let name = name.as_str();
                if matches!(
                    name,
                    "authorization"
                        | "x-api-key"
                        | "origin"
                        | "host"
                        | "content-length"
                        | "content-type"
                ) || name.starts_with("mcp-")
                    || name.starts_with("payment-")
                {
                    return Err(StatusCode::BAD_REQUEST);
                }
            }
        }
        Ok(())
    }
}

impl<S> Layer<S> for TransportLayer {
    type Service = TransportService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        TransportService {
            inner,
            policy: self.clone(),
        }
    }
}

/// Applies the validated transport policy.
#[derive(Debug, Clone)]
pub struct TransportService<S> {
    inner: S,
    policy: TransportLayer,
}

/// A request body with a total deadline; response streams are unaffected.
pub struct DeadlineBody<B> {
    body: Pin<Box<B>>,
    deadline: Pin<Box<tokio::time::Sleep>>,
    expired: Arc<AtomicBool>,
    finished: bool,
}

impl<B: Body<Data = Bytes>> Body for DeadlineBody<B>
where
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        if this.deadline.as_mut().poll(cx).is_ready() {
            this.finished = true;
            this.expired.store(true, Ordering::Relaxed);
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request body deadline exceeded",
            )
            .into())));
        }
        match this.body.as_mut().poll_frame(cx) {
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(frame)) => Poll::Ready(Some(frame.map_err(Into::into))),
            Poll::Pending => Poll::Pending,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.finished || self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// A static, non-cacheable response that never contains request data.
#[must_use]
pub fn rejection(status: StatusCode, code: &'static str) -> Response<MeterBody> {
    let body = Full::new(Bytes::from(format!(r#"{{"error":"{code}"}}"#)))
        .map_err(|never: Infallible| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    secure_headers(&mut response);
    response
}

/// Protect generated rejection responses from shared caching and MIME sniffing.
pub fn secure_headers<B>(response: &mut Response<B>) {
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        http::HeaderValue::from_static("nosniff"),
    );
}

impl<S, B> Service<Request<B>> for TransportService<S>
where
    S: Service<Request<DeadlineBody<B>>, Response = Response<MeterBody>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = Response<MeterBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, request: Request<B>) -> Self::Future {
        if let Err(status) = self.policy.validate(request.headers()) {
            return Box::pin(async move { Ok(rejection(status, "invalid_request_headers")) });
        }
        let replacement = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, replacement);
        let expired = Arc::new(AtomicBool::new(false));
        let (parts, body) = request.into_parts();
        let body = DeadlineBody {
            body: Box::pin(body),
            deadline: Box::pin(tokio::time::sleep(self.policy.body_timeout)),
            expired: expired.clone(),
            finished: false,
        };
        Box::pin(async move {
            let mut response = inner.call(Request::from_parts(parts, body)).await?;
            if expired.load(Ordering::Relaxed) {
                return Ok(rejection(
                    StatusCode::REQUEST_TIMEOUT,
                    "request_body_timeout",
                ));
            }
            if !response.status().is_success() {
                secure_headers(&mut response);
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_connection_validation_is_strict() {
        let policy =
            TransportLayer::new(vec!["https://app.example".into()], Duration::from_secs(30))
                .unwrap();
        let mut headers = HeaderMap::new();
        assert!(policy.validate(&headers).is_ok());
        for origin in ["null", "https://evil.example", "https://app.example/"] {
            headers.insert("origin", origin.parse().unwrap());
            assert_eq!(policy.validate(&headers), Err(StatusCode::FORBIDDEN));
        }
        headers.insert("origin", "https://app.example".parse().unwrap());
        assert!(policy.validate(&headers).is_ok());
        headers.append("origin", "https://app.example".parse().unwrap());
        assert_eq!(policy.validate(&headers), Err(StatusCode::FORBIDDEN));
        headers.clear();
        for name in [
            "Authorization",
            "X-Api-Key",
            "Mcp-Method",
            "Payment-Authorization",
            "Content-Length",
        ] {
            headers.insert("connection", name.parse().unwrap());
            assert_eq!(policy.validate(&headers), Err(StatusCode::BAD_REQUEST));
        }
    }
}
