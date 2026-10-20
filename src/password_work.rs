use std::sync::{Arc, LazyLock};

use axum::http::StatusCode;
use tokio::sync::Semaphore;

use crate::token::TokenError;

const MAX_CONCURRENT: usize = 4;
static SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT)));

pub(crate) async fn run<T, F>(work: F) -> Result<T, TokenError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, TokenError> + Send + 'static,
{
    run_with_slots(SLOTS.clone(), work).await
}

async fn run_with_slots<T, F>(slots: Arc<Semaphore>, work: F) -> Result<T, TokenError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, TokenError> + Send + 'static,
{
    let permit = slots.try_acquire_owned().map_err(|_| {
        TokenError::Request(
            StatusCode::SERVICE_UNAVAILABLE,
            "password service busy; retry later".into(),
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(crate::identity::internal)?
}
