//! Loopback-only operational HTTP surface.
//!
//! This listener is deliberately separate from SSH. It exposes only fixed
//! liveness/readiness responses and aggregate Prometheus text metrics.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::telemetry::Telemetry;

#[derive(Clone)]
struct OperationsState {
    ready: Arc<AtomicBool>,
    telemetry: Telemetry,
}

pub(crate) struct Operations {
    address: SocketAddr,
    state: OperationsState,
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Operations {
    pub(crate) async fn bind(address: SocketAddr, telemetry: Telemetry) -> io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let state = OperationsState {
            ready: Arc::new(AtomicBool::new(false)),
            telemetry,
        };
        let app = Router::new()
            .route("/livez", get(liveness))
            .route("/readyz", get(readiness))
            .route("/metrics", get(metrics))
            .with_state(state.clone());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            if axum::serve(listener, app)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .is_err()
            {
                warn!(
                    operations.failure = "serve",
                    "operations listener stopped with an error"
                );
            }
        });
        Ok(Self {
            address,
            state,
            stop,
            task: Some(task),
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn set_ready(&self, ready: bool) {
        self.state.ready.store(ready, Ordering::Release);
    }

    pub(crate) async fn shutdown(&mut self) {
        self.set_ready(false);
        self.stop.cancel();
        if let Some(task) = self.task.take()
            && task.await.is_err()
        {
            warn!(
                operations.failure = "join",
                "operations listener task failed"
            );
        }
    }
}

impl Drop for Operations {
    fn drop(&mut self) {
        self.set_ready(false);
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn liveness() -> &'static str {
    "live\n"
}

async fn readiness(State(state): State<OperationsState>) -> Response {
    if !state.ready.load(Ordering::Acquire) {
        return (StatusCode::SERVICE_UNAVAILABLE, "not ready\n").into_response();
    }
    if state.telemetry.is_upstream_degraded() {
        (StatusCode::OK, "ready (upstream degraded)\n").into_response()
    } else {
        (StatusCode::OK, "ready\n").into_response()
    }
}

async fn metrics(State(state): State<OperationsState>) -> Response {
    let ready = state.ready.load(Ordering::Acquire);
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.telemetry.render_prometheus(ready),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::*;
    use crate::admission::AdmissionLimits;
    use crate::telemetry::UpstreamStatus;

    #[tokio::test]
    async fn upstream_outage_is_reported_as_degraded_without_losing_readiness() {
        let telemetry = Telemetry::new(&AdmissionLimits::default());
        let mut operations = Operations::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            telemetry.clone(),
        )
        .await
        .expect("operations listener");
        operations.set_ready(true);
        telemetry.upstream_request(UpstreamStatus::TransportError, Duration::from_millis(10));

        let endpoint = format!("http://{}/readyz", operations.local_addr());
        let response = reqwest::get(&endpoint).await.expect("readiness response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.text().await.expect("readiness body"),
            "ready (upstream degraded)\n"
        );

        operations.shutdown().await;
        assert!(
            reqwest::get(endpoint).await.is_err(),
            "operations listener remained reachable after shutdown"
        );
    }
}
