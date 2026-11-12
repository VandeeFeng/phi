use bytes::Bytes;
use futures::StreamExt;
use rig::http_client::{self, HttpClientExt, LazyBody, MultipartForm, Request, Response};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub(crate) struct ProviderTransport(reqwest::Client);

#[derive(Debug)]
struct ResponseBudget {
    remaining: usize,
    exceeded: bool,
    status: Option<u16>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResponseScope(Arc<Mutex<ResponseBudget>>);

tokio::task_local! {
    static RESPONSE_SCOPE: ResponseScope;
}

impl ResponseScope {
    pub(crate) fn new(maximum: usize) -> Self {
        Self(Arc::new(Mutex::new(ResponseBudget {
            remaining: maximum,
            exceeded: false,
            status: None,
        })))
    }

    pub(crate) async fn run<F: std::future::Future>(&self, future: F) -> F::Output {
        RESPONSE_SCOPE.scope(self.clone(), future).await
    }

    pub(crate) fn exceeded_status(&self) -> Option<Option<u16>> {
        let budget = self.0.lock().unwrap();
        budget.exceeded.then_some(budget.status)
    }

    fn charge(&self, bytes: usize) -> http_client::Result<()> {
        let mut budget = self.0.lock().unwrap();
        if budget.exceeded || bytes > budget.remaining {
            budget.exceeded = true;
            return Err(http_client::Error::Instance(Box::new(ResponseLimit)));
        }
        budget.remaining -= bytes;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("provider response byte limit exceeded")]
struct ResponseLimit;

impl HttpClientExt for ProviderTransport {
    fn send<T, U>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let scope = RESPONSE_SCOPE.try_with(Clone::clone).ok();
        let transport = self.clone();
        let request = request.map(Into::into);
        async move {
            let Some(scope) = scope else {
                return transport.0.send(request).await;
            };
            let response = transport.bounded_response(request, scope).await?;
            let (parts, mut stream) = response.into_parts();
            let body: LazyBody<U> = Box::pin(async move {
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    bytes.extend_from_slice(&chunk?);
                }
                Ok(U::from(Bytes::from(bytes)))
            });
            Ok(Response::from_parts(parts, body))
        }
    }

    fn send_multipart<U>(
        &self,
        request: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        let scoped = RESPONSE_SCOPE.try_with(|_| ()).is_ok();
        let response = self.0.send_multipart(request);
        async move {
            if scoped {
                return Err(http_client::Error::StreamEnded);
            }
            response.await
        }
    }

    fn send_streaming<T>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<http_client::StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let scope = RESPONSE_SCOPE.try_with(Clone::clone).ok();
        let request = request.map(Into::into);
        async move {
            match scope {
                Some(scope) => self.bounded_response(request, scope).await,
                None => self.0.send_streaming(request).await,
            }
        }
    }
}

impl ProviderTransport {
    async fn bounded_response(
        &self,
        request: Request<Bytes>,
        scope: ResponseScope,
    ) -> http_client::Result<http_client::StreamingResponse> {
        scope.charge(0)?;
        let (parts, body) = request.into_parts();
        let response = self
            .0
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .map_err(|error| http_client::Error::Instance(Box::new(error)))?;
        let status = response.status();
        scope.0.lock().unwrap().status = Some(status.as_u16());
        let mut builder = Response::builder()
            .status(status)
            .version(response.version());
        *builder.headers_mut().unwrap() = response.headers().clone();
        let stream =
            futures::stream::try_unfold((response, scope), |(mut response, scope)| async move {
                scope.charge(0)?;
                let chunk = response
                    .chunk()
                    .await
                    .map_err(|error| http_client::Error::Instance(Box::new(error)))?;
                match chunk {
                    Some(chunk) => {
                        scope.charge(chunk.len())?;
                        Ok(Some((chunk, (response, scope))))
                    }
                    None => Ok(None),
                }
            });
        let mut stream: http_client::sse::BoxedStream = Box::pin(stream);
        if !status.is_success() {
            let headers = Box::new(builder.headers_ref().unwrap().clone());
            // Never ask reqwest or Rig to buffer an unbounded diagnostic body.
            while let Some(chunk) = stream.next().await {
                chunk?;
            }
            return Err(http_client::Error::InvalidStatusCodeWithDetails {
                status,
                body: String::new(),
                headers,
            });
        }
        builder.body(stream).map_err(http_client::Error::Protocol)
    }
}
