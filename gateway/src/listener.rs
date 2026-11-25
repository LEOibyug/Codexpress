use axum::Router;
use std::{io, net::Ipv4Addr};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

pub(crate) struct Listener {
    port: u16,
    task: JoinHandle<()>,
    // Replacing or clearing this listener drops the sender and starts graceful shutdown.
    _shutdown: oneshot::Sender<()>,
}

impl Listener {
    pub(crate) fn running(&self) -> bool {
        !self.task.is_finished()
    }
}

pub(crate) enum ListenerChange {
    Keep,
    Stop,
    Start(TcpListener, u16),
}

impl ListenerChange {
    // Reserve a new port before saving settings, but serve only after publication.
    pub(crate) async fn prepare(
        current: Option<&Listener>,
        enabled: bool,
        address: Ipv4Addr,
        port: u16,
    ) -> io::Result<Self> {
        if !enabled {
            Ok(Self::Stop)
        } else if current.is_some_and(|l| l.port == port && l.running()) {
            Ok(Self::Keep)
        } else {
            Ok(Self::Start(TcpListener::bind((address, port)).await?, port))
        }
    }

    pub(crate) fn apply(self, current: &mut Option<Listener>, router: impl FnOnce() -> Router) {
        *current = match self {
            Self::Keep => return,
            Self::Stop => None,
            Self::Start(socket, port) => {
                let (shutdown, stopped) = oneshot::channel();
                let app = router();
                let task = tokio::spawn(async move {
                    let _ = axum::serve(socket, app)
                        .with_graceful_shutdown(async { let _ = stopped.await; })
                        .await;
                });
                Some(Listener { port, task, _shutdown: shutdown })
            }
        };
    }
}
