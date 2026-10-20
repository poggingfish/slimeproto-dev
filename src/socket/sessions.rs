use std::{
    collections::HashMap,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::oneshot;

type Registry = HashMap<Vec<u8>, HashMap<u64, oneshot::Sender<()>>>;
static SESSIONS: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(super) struct SessionGuard {
    key: Vec<u8>,
    id: u64,
}

impl SessionGuard {
    pub(super) fn register(key: Vec<u8>) -> (Self, oneshot::Receiver<()>) {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        SESSIONS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key.clone())
            .or_default()
            .insert(id, sender);
        (Self { key, id }, receiver)
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut registry = SESSIONS.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(sockets) = registry.get_mut(&self.key) {
            sockets.remove(&self.id);
            if sockets.is_empty() {
                registry.remove(&self.key);
            }
        }
    }
}

pub(crate) fn revoke_session(key: &[u8]) {
    let sockets = SESSIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(key);
    if let Some(sockets) = sockets {
        for sender in sockets.into_values() {
            let _ = sender.send(());
        }
    }
}
