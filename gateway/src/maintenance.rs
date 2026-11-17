use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use serde_json::{json, Value};
use std::{
    pin::Pin,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    task::{Context, Poll},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::model::AppState;

const PERMITS: u32 = 1 << 20;
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn info() -> Value {
    json!({"version": VERSION, "protocol": 1})
}

pub(crate) struct Maintenance {
    work: Arc<Semaphore>,
    hold_ingress: AtomicBool,
    pub(crate) shutdown: Notify,
}

impl Default for Maintenance {
    fn default() -> Self {
        Self {
            work: Arc::new(Semaphore::new(PERMITS as usize)),
            hold_ingress: AtomicBool::new(false),
            shutdown: Notify::new(),
        }
    }
}

impl Maintenance {
    pub(crate) fn enter(&self) -> Option<OwnedSemaphorePermit> {
        self.work.clone().try_acquire_owned().ok()
    }

    pub(crate) fn begin_shutdown(&self) -> Option<OwnedSemaphorePermit> {
        self.work.clone().try_acquire_many_owned(PERMITS).ok()
    }

    pub(crate) fn hold_ingress(&self, hold: bool) {
        self.hold_ingress.store(hold, Ordering::Release);
    }
}

pub(crate) async fn http(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let permit = (!state.maintenance.hold_ingress.load(Ordering::Acquire))
        .then(|| state.maintenance.enter()).flatten();
    let Some(permit) = permit else {
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "1")], Json(json!({
            "error": {"code": "gateway_updating", "message": "网关正在切换版本，请稍后重试"}
        }))).into_response();
    };
    next.run(request).await.map(|inner| Body::new(ActiveBody { inner, _permit: permit }))
}

struct ActiveBody {
    inner: Body,
    _permit: OwnedSemaphorePermit,
}

impl HttpBody for ActiveBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool { self.inner.is_end_stream() }
    fn size_hint(&self) -> SizeHint { self.inner.size_hint() }
}
