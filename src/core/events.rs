//! Session event history and fan-out.
//!
//! Every provider axis keeps the same two things per session: a bounded
//! history of events, and a broadcast channel that live SSE subscribers read
//! from. The helpers here are the whole contract.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

/// Maximum number of events retained per session history.
///
/// History exists to replay context to a late SSE subscriber, not to be a
/// durable log — older events are dropped once the cap is reached.
pub const MAX_HISTORY: usize = 500;

/// An event together with its position in the session's stream.
///
/// The number is assigned once, when the event is recorded, and is never
/// reused. That is what makes it usable as an SSE event id: a client can hand
/// it back as `Last-Event-ID` and get exactly what it missed. A position in the
/// history buffer could not do this — the buffer drops its oldest entries past
/// [`MAX_HISTORY`], so positions shift under a resuming client.
#[derive(Debug)]
pub struct Seq<E> {
    pub seq: u64,
    pub event: Arc<E>,
}

// Derived Clone would demand `E: Clone`; only the Arc is cloned here.
impl<E> Clone for Seq<E> {
    fn clone(&self) -> Self {
        Self {
            seq: self.seq,
            event: self.event.clone(),
        }
    }
}

/// What a client wants replayed when it attaches to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replay {
    /// Every event still held in history. A fresh page rendering the whole
    /// conversation wants this.
    #[default]
    All,
    /// Only the most recent event — enough to show where the session is now
    /// without paying for the backlog.
    Last,
    /// Nothing; follow live traffic only.
    None,
}

impl Replay {
    /// Trim a history snapshot to what this policy asks for.
    fn apply<E>(self, history: Vec<Seq<E>>) -> Vec<Seq<E>> {
        match self {
            Replay::All => history,
            Replay::Last => history.into_iter().next_back().into_iter().collect(),
            Replay::None => Vec::new(),
        }
    }
}

/// Resolve what to replay from the request.
///
/// `Last-Event-ID` wins when present: it is a resume of a known position, which
/// is more specific than an attach policy. A malformed value is ignored rather
/// than rejected — a browser sends this header on its own during automatic
/// reconnects, and failing the stream over it would be worse than replaying.
pub fn resume_point(headers: &HeaderMap, replay: Replay) -> ResumeFrom {
    match headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(seq) => ResumeFrom::After(seq),
        None => ResumeFrom::Policy(replay),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeFrom {
    /// Replay only events recorded after this sequence number.
    After(u64),
    /// No resume point — use the attach policy.
    Policy(Replay),
}

impl ResumeFrom {
    fn select<E>(self, history: Vec<Seq<E>>) -> Vec<Seq<E>> {
        match self {
            ResumeFrom::After(seq) => history.into_iter().filter(|e| e.seq > seq).collect(),
            ResumeFrom::Policy(p) => p.apply(history),
        }
    }
}

/// Append `event` to `history` (trimming to [`MAX_HISTORY`]) and publish it to
/// live subscribers, stamping it with the session's next sequence number.
///
/// The history lock is released before the broadcast so a slow or contended
/// subscriber can never block a producer that is appending.
///
/// A send error means there are no subscribers, which is normal — the event is
/// already recorded in history and a later subscriber will replay it.
pub async fn record_and_broadcast<E>(
    history: &Mutex<VecDeque<Seq<E>>>,
    event_tx: &broadcast::Sender<Seq<E>>,
    next_seq: &AtomicU64,
    event: Arc<E>,
) {
    let stamped = Seq {
        seq: next_seq.fetch_add(1, Ordering::Relaxed),
        event,
    };
    {
        let mut history = history.lock().await;
        history.push_back(stamped.clone());
        while history.len() > MAX_HISTORY {
            history.pop_front();
        }
    }
    let _ = event_tx.send(stamped);
}

/// Build an SSE response that replays part of `history` and then follows `rx`
/// live.
///
/// Each frame carries its sequence number as the SSE event id, so a client that
/// drops can resume with `Last-Event-ID` and receive only what it missed.
///
/// The stream ends when the broadcast channel closes (the session's producer is
/// gone). A lagged subscriber gets a `stream_lagged` error event and keeps
/// going rather than being dropped — losing events is preferable to killing an
/// otherwise healthy connection.
pub fn sse_replay_then_follow<E>(
    history: Vec<Seq<E>>,
    mut rx: broadcast::Receiver<Seq<E>>,
    from: ResumeFrom,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>>
where
    E: Serialize + Send + Sync + 'static,
{
    let replayed = from.select(history);

    let stream = async_stream::stream! {
        // Events already delivered by the replay must not be sent again when
        // the live tail catches up: a subscriber opened before the snapshot
        // was taken still has them queued.
        let mut last_sent = replayed.last().map(|e| e.seq);

        for entry in replayed {
            if let Ok(data) = serde_json::to_string(entry.event.as_ref()) {
                yield Ok::<_, axum::Error>(Event::default().id(entry.seq.to_string()).data(data));
            }
        }

        loop {
            match rx.recv().await {
                Ok(entry) => {
                    if last_sent.is_some_and(|seen| entry.seq <= seen) {
                        continue;
                    }
                    last_sent = Some(entry.seq);
                    if let Ok(data) = serde_json::to_string(entry.event.as_ref()) {
                        yield Ok(Event::default().id(entry.seq.to_string()).data(data));
                    }
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

    async fn log(
        n: u64,
    ) -> (
        Mutex<VecDeque<Seq<u64>>>,
        broadcast::Sender<Seq<u64>>,
        AtomicU64,
    ) {
        let history = Mutex::new(VecDeque::new());
        let (tx, _rx) = broadcast::channel(64);
        let seq = AtomicU64::new(0);
        for i in 0..n {
            record_and_broadcast(&history, &tx, &seq, Arc::new(i)).await;
        }
        (history, tx, seq)
    }

    async fn snapshot(history: &Mutex<VecDeque<Seq<u64>>>) -> Vec<Seq<u64>> {
        history.lock().await.iter().cloned().collect()
    }

    #[tokio::test]
    async fn history_is_capped_and_keeps_the_newest_events() {
        let (history, _tx, _seq) = log(MAX_HISTORY as u64 + 10).await;

        let history = history.lock().await;
        assert_eq!(history.len(), MAX_HISTORY);
        assert_eq!(*history.front().unwrap().event, 10);
        assert_eq!(*history.back().unwrap().event, MAX_HISTORY as u64 + 9);
    }

    /// Sequence numbers must survive the buffer dropping its oldest entries —
    /// that is the whole reason they are not history positions.
    #[tokio::test]
    async fn sequence_numbers_keep_counting_past_the_cap() {
        let (history, _tx, _seq) = log(MAX_HISTORY as u64 + 10).await;

        let history = history.lock().await;
        assert_eq!(history.front().unwrap().seq, 10);
        assert_eq!(history.back().unwrap().seq, MAX_HISTORY as u64 + 9);
    }

    #[tokio::test]
    async fn events_reach_live_subscribers_with_their_sequence() {
        let history = Mutex::new(VecDeque::new());
        let (tx, mut rx) = broadcast::channel(4);
        let seq = AtomicU64::new(0);

        record_and_broadcast(&history, &tx, &seq, Arc::new(7u64)).await;

        let got = rx.recv().await.unwrap();
        assert_eq!(got.seq, 0);
        assert_eq!(*got.event, 7);
    }

    /// No subscribers must not be treated as a failure — the event still lands
    /// in history for a later subscriber to replay.
    #[tokio::test]
    async fn recording_succeeds_without_subscribers() {
        let history = Mutex::new(VecDeque::new());
        let (tx, rx) = broadcast::channel(4);
        drop(rx);

        record_and_broadcast(&history, &tx, &AtomicU64::new(0), Arc::new(1u64)).await;

        assert_eq!(history.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn resume_replays_only_what_came_after() {
        let (history, _tx, _seq) = log(5).await;
        let snap = snapshot(&history).await;

        let picked = ResumeFrom::After(2).select(snap);

        assert_eq!(picked.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4]);
    }

    /// A resume point past the end is not an error — the client is already
    /// current and simply has nothing to catch up on.
    #[tokio::test]
    async fn resume_past_the_end_replays_nothing() {
        let (history, _tx, _seq) = log(3).await;
        let snap = snapshot(&history).await;

        assert!(ResumeFrom::After(99).select(snap).is_empty());
    }

    #[tokio::test]
    async fn attach_policies_select_the_documented_slice() {
        let (history, _tx, _seq) = log(4).await;

        let all = Replay::All.apply(snapshot(&history).await);
        assert_eq!(
            all.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );

        let last = Replay::Last.apply(snapshot(&history).await);
        assert_eq!(last.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3]);

        assert!(Replay::None.apply(snapshot(&history).await).is_empty());
    }

    #[tokio::test]
    async fn last_on_an_empty_history_is_empty() {
        let (history, _tx, _seq) = log(0).await;

        assert!(Replay::Last.apply(snapshot(&history).await).is_empty());
    }

    #[test]
    fn a_resume_header_outranks_the_attach_policy() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "12".parse().unwrap());

        assert_eq!(resume_point(&headers, Replay::None), ResumeFrom::After(12));
    }

    /// Browsers send this header themselves on reconnect; a value we cannot
    /// read must not fail the stream.
    #[test]
    fn an_unreadable_resume_header_falls_back_to_the_policy() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "not-a-number".parse().unwrap());

        assert_eq!(
            resume_point(&headers, Replay::Last),
            ResumeFrom::Policy(Replay::Last)
        );
    }

    #[test]
    fn no_resume_header_uses_the_attach_policy() {
        assert_eq!(
            resume_point(&HeaderMap::new(), Replay::All),
            ResumeFrom::Policy(Replay::All)
        );
    }
}
