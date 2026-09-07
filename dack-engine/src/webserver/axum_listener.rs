//! `axum` webhook listener — the inbound HTTP surface for `webhook`-triggered
//! duties, bound **localhost-only**. A matched `POST <path>` emits a [`FiredTrigger`] (the
//! request body as `public`-tier payload) onto the shared channel the harness drains; the
//! body is **data, never a script**, so a webhook can wake a duty but cannot
//! introduce one. The route table hot-reloads from the registry via [`set_routes`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::Router;
use tokio::sync::mpsc;

use super::WebhookListener;
use crate::error::{DackError, Result};
use crate::sources::FiredTrigger;

/// Shared, hot-reloadable state behind the HTTP handler.
struct Inner {
    /// `path` → `def_id`. Replaced wholesale on `set_routes`.
    routes: RwLock<HashMap<String, String>>,
    tx: mpsc::Sender<FiredTrigger>,
}

pub struct AxumWebhookListener {
    addr: SocketAddr,
    inner: Arc<Inner>,
}

/// Max accepted webhook body (1 MiB) — a wake signal, not a bulk-upload endpoint.
const MAX_BODY: usize = 1 << 20;

impl AxumWebhookListener {
    /// Build the listener over a caller-owned `FiredTrigger` channel (shared with the cron
    /// wheel). Bind/serve happens in [`serve`](Self::serve).
    pub fn new(addr: SocketAddr, tx: mpsc::Sender<FiredTrigger>) -> Arc<Self> {
        Arc::new(Self {
            addr,
            inner: Arc::new(Inner {
                routes: RwLock::new(HashMap::new()),
                tx,
            }),
        })
    }

    /// Bind localhost and serve until the process ends. Caller owns the task handle.
    pub async fn serve(self: Arc<Self>) -> Result<()> {
        let app = Router::new()
            .fallback(handle)
            .with_state(self.inner.clone());
        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .map_err(|e| DackError::Config(format!("webhook bind {}: {e}", self.addr)))?;
        axum::serve(listener, app)
            .await
            .map_err(|e| DackError::Config(format!("webhook serve: {e}")))?;
        Ok(())
    }
}

/// Catch-all handler: POST to a registered path → a `FiredTrigger`; anything else → 404/405.
async fn handle(State(inner): State<Arc<Inner>>, req: Request) -> StatusCode {
    if req.method() != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED;
    }
    let path = req.uri().path().to_string();
    let def_id = inner.routes.read().unwrap().get(&path).cloned();
    let Some(def_id) = def_id else {
        return StatusCode::NOT_FOUND;
    };
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b.to_vec(),
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    match inner
        .tx
        .send(FiredTrigger {
            def_id,
            payload: body,
        })
        .await
    {
        Ok(()) => StatusCode::ACCEPTED,
        // Receiver gone → harness shutting down.
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[async_trait::async_trait]
impl WebhookListener for AxumWebhookListener {
    async fn set_routes(&self, routes: &[(String, String)]) -> Result<()> {
        *self.inner.routes.write().unwrap() = routes.iter().cloned().collect();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn listener() -> (Arc<AxumWebhookListener>, mpsc::Receiver<FiredTrigger>) {
        let (tx, rx) = mpsc::channel(8);
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        (AxumWebhookListener::new(addr, tx), rx)
    }

    fn post(path: &str, body: &str) -> Request {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn matched_post_emits_a_fired_trigger() {
        let (l, mut rx) = listener();
        l.set_routes(&[("/hooks/inbox".into(), "inbox".into())])
            .await
            .unwrap();

        let status = handle(State(l.inner.clone()), post("/hooks/inbox", "ping")).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        let fired = rx.try_recv().expect("a trigger was emitted");
        assert_eq!(fired.def_id, "inbox");
        assert_eq!(fired.payload, b"ping");
    }

    #[tokio::test]
    async fn unknown_path_is_404_and_emits_nothing() {
        let (l, mut rx) = listener();
        l.set_routes(&[("/hooks/inbox".into(), "inbox".into())])
            .await
            .unwrap();
        let status = handle(State(l.inner.clone()), post("/nope", "x")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_post_is_rejected() {
        let (l, _rx) = listener();
        l.set_routes(&[("/hooks/inbox".into(), "inbox".into())])
            .await
            .unwrap();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/hooks/inbox")
            .body(Body::empty())
            .unwrap();
        assert_eq!(handle(State(l.inner.clone()), req).await, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn routes_hot_reload() {
        let (l, mut rx) = listener();
        l.set_routes(&[("/old".into(), "d".into())]).await.unwrap();
        l.set_routes(&[("/new".into(), "d".into())]).await.unwrap(); // replaces, not merges
        assert_eq!(handle(State(l.inner.clone()), post("/old", "x")).await, StatusCode::NOT_FOUND);
        assert_eq!(handle(State(l.inner.clone()), post("/new", "x")).await, StatusCode::ACCEPTED);
        assert_eq!(rx.try_recv().unwrap().def_id, "d");
    }
}
