//! HTTPS-only NRD endpoint recovery. Request semantics are defined in api.rs;
//! DNS, TLS and HTTP framing remain the HTTP library's responsibility.

use std::{
    error::Error as _,
    io,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use reqwest::{Client, Method, StatusCode, header::HeaderMap};

pub(crate) const PRIMARY: &str = "https://api.nrd.nie.163.com";
const SECONDARY: &str = "https://api-dcdn.nrd.nie.163.com";

#[derive(Default)]
struct Route {
    index: usize,
    generation: u32,
    requests: u32,
}

#[derive(Clone)]
pub(crate) struct NrdHttp {
    client: Client,
    endpoints: [String; 2],
    route: Arc<Mutex<Route>>,
}

impl NrdHttp {
    pub(crate) fn new() -> Result<Self> {
        // Only routing values are process-wide. The HTTP client/connection pool
        // is owned by the API caller, not leaked through a global client.
        static ROUTE: OnceLock<Arc<Mutex<Route>>> = OnceLock::new();
        Ok(Self {
            client: Client::builder()
                .https_only(true)
                .http1_only()
                .redirect(reqwest::redirect::Policy::none())
                .gzip(true)
                .user_agent(concat!("OpenUUYC/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("build NRD HTTPS client")?,
            endpoints: [PRIMARY.to_owned(), SECONDARY.to_owned()],
            route: Arc::clone(ROUTE.get_or_init(Default::default)),
        })
    }

    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: Vec<u8>,
        timeout: Duration,
        replay_ambiguous: bool,
    ) -> Result<(StatusCode, Bytes)> {
        let generation = {
            let mut route = self
                .route
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            route.requests = route.requests.wrapping_add(1);
            if route.requests != 0 && route.requests.is_multiple_of(100) && route.index != 0 {
                route.index = 0;
                route.generation = route.generation.wrapping_add(1);
                tracing::debug!("NRD HTTPS routing returned to primary endpoint");
            }
            route.generation
        };

        // Re-read the shared route on every attempt. A concurrent primary probe
        // must not be undone by an old failure; bound even that interleaving.
        for attempt in 0..=4 {
            let index = self
                .route
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .index;
            let endpoint = &self.endpoints[index];
            let mut request = self
                .client
                .request(method.clone(), format!("{endpoint}{path}"))
                .headers(headers.clone())
                .timeout(timeout);
            if method == Method::POST || method == Method::PUT {
                request = request.body(body.clone());
            }
            tracing::trace!(path, %method, endpoint, attempt, "sending NRD HTTPS request");
            let response = request.send().await;
            let outcome = match response {
                Ok(response) => {
                    let status = response.status();
                    match response.bytes().await {
                        Ok(bytes) => {
                            if status == StatusCode::NOT_FOUND
                                && self.advance(index, generation, attempt)
                            {
                                tracing::warn!(
                                    path,
                                    endpoint,
                                    attempt,
                                    "NRD HTTPS 404; trying backup endpoint"
                                );
                                continue;
                            }
                            return Ok((status, bytes));
                        }
                        Err(error) => (error, false),
                    }
                }
                Err(error) => (error, true),
            };
            let (error, before_response) = outcome;
            if (replay_ambiguous || error.is_connect())
                && retryable_transport_error(&error, before_response)
                && self.advance(index, generation, attempt)
            {
                tracing::warn!(path, endpoint, attempt, %error, "NRD HTTPS transport failed; trying backup endpoint");
                continue;
            }
            return Err(error).with_context(|| {
                format!(
                    "NRD HTTPS request failed ({endpoint}{path}, attempt {})",
                    attempt + 1
                )
            });
        }
        unreachable!("the last attempt always returns its response or error")
    }

    fn advance(&self, index: usize, generation: u32, attempt: usize) -> bool {
        if attempt >= 4 || index + 1 >= self.endpoints.len() {
            return false;
        }
        let mut route = self
            .route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if route.generation == generation && route.index < index + 1 {
            route.index = index + 1;
        }
        true
    }
}

fn retryable_transport_error(error: &reqwest::Error, before_response: bool) -> bool {
    if error.is_connect() || error.is_timeout() {
        return true;
    }
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(http) = cause.downcast_ref::<hyper::Error>() {
            // An incomplete body is not an empty response. Never turn a body
            // parse/decompression failure into a replay of a state-changing POST.
            if http.is_parse() || http.is_user() {
                return false;
            }
            if before_response && (http.is_incomplete_message() || http.is_closed()) {
                return true;
            }
        }
        if let Some(io) = cause.downcast_ref::<io::Error>()
            && (matches!(
                io.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::NotConnected
            ) || (before_response && io.kind() == io::ErrorKind::UnexpectedEof))
        {
            return true;
        }
        source = cause.source();
    }
    false
}
