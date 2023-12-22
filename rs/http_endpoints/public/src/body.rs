use crate::common::{make_plaintext_response, poll_ready};
use byte_unit::Byte;
use bytes::Bytes;
use http::Request;
use hyper::{Body, Response, StatusCode};
use ic_async_utils::{receive_body, BodyReceiveError};
use ic_config::http_handler::Config;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tower::{BoxError, Layer, Service};

pub(crate) struct BodyReceiverLayer {
    max_request_receive_duration: Duration,
    max_request_body_size: Byte,
}

impl BodyReceiverLayer {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            max_request_receive_duration: Duration::from_secs(config.max_request_receive_seconds),
            max_request_body_size: Byte::from_bytes(config.max_request_size_bytes.into()),
        }
    }
}

impl<S> Layer<S> for BodyReceiverLayer {
    type Service = BodyReceiverService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BodyReceiverService {
            max_request_receive_duration: self.max_request_receive_duration,
            max_request_body_size_bytes: self.max_request_body_size,
            inner,
        }
    }
}

#[derive(Clone)]
pub(crate) struct BodyReceiverService<S> {
    max_request_receive_duration: Duration,
    max_request_body_size_bytes: Byte,
    inner: S,
}

impl<S> Service<Request<Body>> for BodyReceiverService<S>
where
    S: Service<
            Request<Bytes>,
            Response = Response<Body>,
            Error = Infallible,
            Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>,
        > + Clone
        + Send
        + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        poll_ready(self.inner.poll_ready(cx))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let inner = self.inner.clone();

        // In case the inner service has state that's driven to readiness and
        // not tracked by clones (such as `Buffer`), pass the version we have
        // already called `poll_ready` on into the future, and leave its clone
        // behind.
        //
        // The types implementing the Service trait are not necessary thread-safe.
        // So the unless the caller is sure that the service implementation is
        // thread-safe we must make sure 'poll_ready' is always called before 'call'
        // on the same object. Hence if 'poll_ready' is called and not tracked by
        // the 'Clone' implementation the following sequence of events may panic.
        //
        //  s1.call_ready()
        //  s2 = s1.clone()
        //  s2.call()
        let mut inner = std::mem::replace(&mut self.inner, inner);

        let max_request_receive_duration = self.max_request_receive_duration;
        let max_request_body_size_bytes = self.max_request_body_size_bytes;
        let (parts, body) = request.into_parts();
        Box::pin(async move {
            match receive_body(
                body,
                max_request_receive_duration,
                max_request_body_size_bytes,
            )
            .await
            {
                Err(err) => match err {
                    BodyReceiveError::TooLarge(e) => {
                        Ok(make_plaintext_response(StatusCode::PAYLOAD_TOO_LARGE, e))
                    }
                    BodyReceiveError::Timeout(e) => {
                        Ok(make_plaintext_response(StatusCode::REQUEST_TIMEOUT, e))
                    }
                    BodyReceiveError::Unavailable(e) => {
                        Ok(make_plaintext_response(StatusCode::BAD_REQUEST, e))
                    }
                },
                Ok(body) => Ok(inner
                    .call(Request::from_parts(parts, body))
                    .await
                    .expect("Can't panic on infallible.")),
            }
        })
    }
}
