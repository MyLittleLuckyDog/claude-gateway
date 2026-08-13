//! Session event history and fan-out.
//!
//! Every provider axis keeps the same two things per session: a bounded
//! history of events, and a broadcast channel that live SSE subscribers read
//! from. The two helpers here are the whole contract.

use std::collections::VecDeque;
use std::sync::Arc;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::Serialize;
use tokio::sync::{broadcast, Mutex};

/// Maximum number of events retained per session history.
///
/// History exists to replay context to a late SSE subscriber, not to be a
/// durable log — older events are dropped once the cap is reached.
pub const MAX_HISTORY: usize = 500;

/// Append `event` to `history` (trimming to [`MAX_HISTORY`]) and publish it to
/// live subscribers.
///
/// The history lock is released before the broadcast so a slow or contended
/// subscriber can never block a producer that is appending.
///
/// A send error means there are no subscribers, which is normal — the event is
/// already recorded in history and a later subscriber will replay it.
pub async fn record_and_broadcast<E>(
    history: &Mutex<VecDeque<Arc<E>>>,
    event_tx: &broadcast::Sender<Arc<E>>,
    event: Arc<E>,
) {
    {
        let mut history = history.lock().await;
        history.push_back(event.clone());
        while history.len() > MAX_HISTORY {
            history.pop_front();
        }
    }
    let _ = event_tx.send(event);
}

/// Build an SSE response that replays `history` and then follows `rx` live.
///
/// Event IDs are the running index, continuing from the end of the replayed
/// history so a client can tell replay from live traffic.
///
/// The stream ends when the broadcast channel closes (the session's producer
/// is gone). A lagged subscriber gets a `stream_lagged` error event and keeps
/// going rather than being dropped — losing events is preferable to killing an
/// otherwise healthy connection.
pub fn sse_replay_then_follow<E>(
    history: Vec<Arc<E>>,
    mut rx: broadcast::Receiver<Arc<E>>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>>
where
    E: Serialize + Send + Sync + 'static,
{
    let stream = async_stream::stream! {
        let mut idx = 0usize;

        for event in &history {
            if let Ok(data) = serde_json::to_string(event.as_ref()) {
                yield Ok::<_, axum::Error>(Event::default().id(idx.to_string()).data(data));
            }
            idx += 1;
        }

        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Ok(data) = serde_json::to_string(event.as_ref()) {
                        yield Ok(Event::default().id(idx.to_string()).data(data));
                    }
                    idx += 1;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let err = serde_json::json!({
                        "type": "error",
                        "message": format!("Lagged: {} events skipped", n),
                        "code": "stream_lagged",
                    });
                    yield Ok(Event::default().data(err.to_string()));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn history_is_capped_and_keeps_the_newest_events() {
        let history = Mutex::new(VecDeque::new());
        let (tx, _rx) = broadcast::channel(16);

        for i in 0..(MAX_HISTORY + 10) {
            record_and_broadcast(&history, &tx, Arc::new(i)).await;
        }

        let history = history.lock().await;
        assert_eq!(history.len(), MAX_HISTORY);
        assert_eq!(**history.front().unwrap(), 10);
        assert_eq!(**history.back().unwrap(), MAX_HISTORY + 9);
    }

    #[tokio::test]
    async fn events_reach_live_subscribers() {
        let history = Mutex::new(VecDeque::new());
        let (tx, mut rx) = broadcast::channel(4);

        record_and_broadcast(&history, &tx, Arc::new("hello")).await;

        assert_eq!(*rx.recv().await.unwrap(), "hello");
        assert_eq!(history.lock().await.len(), 1);
    }

    /// No subscribers must not be treated as a failure — the event still lands
    /// in history for a later subscriber to replay.
    #[tokio::test]
    async fn recording_succeeds_without_subscribers() {
        let history = Mutex::new(VecDeque::new());
        let (tx, rx) = broadcast::channel(4);
        drop(rx);

        record_and_broadcast(&history, &tx, Arc::new(1)).await;

        assert_eq!(history.lock().await.len(), 1);
    }
}
