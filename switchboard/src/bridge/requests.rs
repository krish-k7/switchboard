//! Three bounded worker lanes for registration, send, status, and pong
//! requests; accepted requests run once or are cancelled.
//!
//! Lane 0 is reserved for registration (one queued slot); message work uses
//! lanes 1 and 2 (32 slots each), chosen by the first byte of the SHA-256 of
//! the message ID, so work for one message stays in order. Completion runs
//! on the worker (or the cancelling caller) and must only enqueue.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::hub_transport::HubError;
use crate::wirejson;

/// `(generation, uuid4().hex, kind)`.
pub type Tag = (u64, String, &'static str);

pub type Completion = dyn Fn(Tag, Option<Map<String, Value>>, Option<HubError>) + Send + Sync;

/// One server request: `(path, body) -> reply` (the shared `HubClient` in production).
pub type Post =
    Arc<dyn Fn(&str, &Value) -> std::result::Result<Map<String, Value>, HubError> + Send + Sync>;

const LIMITS: [usize; 3] = [1, 32, 32];

struct State {
    queues: [VecDeque<(String, Value, Tag)>; 3],
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    work: Condvar,
    post: Post,
    completion: Box<Completion>,
}

pub struct HubRequests {
    shared: Arc<Shared>,
}

/// The fixed path for each request kind.
pub fn expected_path(kind: &str) -> Option<&'static str> {
    match kind {
        "register" => Some("/v1/register"),
        "send" => Some("/v1/send"),
        "status" => Some("/v1/status"),
        "pong" => Some("/v1/pong"),
        _ => None,
    }
}

impl HubRequests {
    pub fn new(post: Post, completion: Box<Completion>) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queues: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
                closed: false,
            }),
            work: Condvar::new(),
            post,
            completion,
        });
        for lane in 0..3 {
            let worker = shared.clone();
            std::thread::Builder::new()
                .name(format!("switchboard-server-request-{lane}"))
                .stack_size(256 * 1024)
                .spawn(move || run(&worker, lane))
                .map_err(|exc| Error::from_io(&exc, None))?;
        }
        Ok(HubRequests { shared })
    }

    /// Queue one request; Ok(false) when the lane is full or closed.
    pub fn submit(
        &self,
        kind: &'static str,
        path: &str,
        body: &Value,
        tag: Tag,
        key: Option<&str>,
    ) -> Result<bool> {
        if expected_path(kind) != Some(path) {
            return Err(Error::value("invalid server request kind/path"));
        }
        let encoded = wirejson::dumps(body);
        if encoded.len() > 65536 {
            return Err(Error::value("server request too large"));
        }
        let snapshot = body.clone();
        let lane = if kind == "register" {
            0
        } else {
            let key = match key {
                Some(key) => Some(key.to_string()),
                None => snapshot
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            let key = match key {
                Some(key) if !key.is_empty() => key,
                _ => {
                    return Err(Error::value(
                        "message request requires a nonempty ordering key",
                    ))
                }
            };
            1 + (Sha256::digest(key.as_bytes())[0] & 1) as usize
        };
        let mut state = self.shared.state.lock().expect("requests");
        if state.closed || state.queues[lane].len() >= LIMITS[lane] {
            return Ok(false);
        }
        state.queues[lane].push_back((path.to_string(), snapshot, tag));
        self.shared.work.notify_all();
        Ok(true)
    }

    /// Cancel queued tags matching `predicate`; returns their count. Work
    /// past the dequeue boundary may already have reached the server.
    pub fn cancel(&self, predicate: impl Fn(&Tag) -> bool) -> usize {
        let cancelled = {
            let mut state = self.shared.state.lock().expect("requests");
            remove_queued(&mut state, &predicate)
        };
        let count = cancelled.len();
        report_cancelled(&self.shared, cancelled);
        count
    }

    /// Cancel queued work promptly; already-started calls complete once.
    pub fn close(&self) {
        let cancelled = {
            let mut state = self.shared.state.lock().expect("requests");
            state.closed = true;
            let cancelled = remove_queued(&mut state, &|_| true);
            self.shared.work.notify_all();
            cancelled
        };
        report_cancelled(&self.shared, cancelled);
    }
}

fn remove_queued(state: &mut State, predicate: &dyn Fn(&Tag) -> bool) -> Vec<Tag> {
    let mut cancelled = Vec::new();
    for queue in state.queues.iter_mut() {
        let mut kept = VecDeque::new();
        for job in queue.drain(..) {
            if predicate(&job.2) {
                cancelled.push(job.2);
            } else {
                kept.push_back(job);
            }
        }
        *queue = kept;
    }
    cancelled
}

fn report_cancelled(shared: &Shared, cancelled: Vec<Tag>) {
    for tag in cancelled {
        (shared.completion)(tag, None, Some(HubError::Cancelled));
    }
}

fn run(shared: &Shared, lane: usize) {
    loop {
        let (path, body, tag) = {
            let mut state = shared.state.lock().expect("requests");
            while !state.closed && state.queues[lane].is_empty() {
                state = shared.work.wait(state).expect("requests");
            }
            if state.closed {
                return;
            }
            state.queues[lane].pop_front().expect("queued work")
        };
        match (shared.post)(&path, &body) {
            Ok(value) => (shared.completion)(tag, Some(value), None),
            Err(error) => (shared.completion)(tag, None, Some(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};
    use std::time::Duration;

    use crate::bridge::queue::Flag;
    use serde_json::json;

    type Seen = Receiver<(Tag, Option<Map<String, Value>>, Option<HubError>)>;

    fn tag(label: &str) -> Tag {
        (0, label.to_string(), "send")
    }

    fn start(post: Post) -> (HubRequests, Seen) {
        let (sender, receiver) = channel();
        let sender = Mutex::new(sender);
        let requests = HubRequests::new(
            post,
            Box::new(move |tag, result, error| {
                let _ = sender.lock().unwrap().send((tag, result, error));
            }),
        )
        .unwrap();
        (requests, receiver)
    }

    fn keys() -> (String, String) {
        let lane = |key: &str| Sha256::digest(key.as_bytes())[0] & 1;
        let a = "message-a".to_string();
        let b = (0..20)
            .map(|i| i.to_string())
            .find(|k| lane(k) != lane(&a))
            .unwrap();
        (a, b)
    }

    fn next(seen: &Seen) -> (Tag, Option<Map<String, Value>>, Option<HubError>) {
        seen.recv_timeout(Duration::from_secs(2))
            .expect("completion")
    }

    #[test]
    fn blocked_message_does_not_block_other_lane_or_registration() {
        let (a, b) = keys();
        let (entered, release) = (Arc::new(Flag::new()), Arc::new(Flag::new()));
        let (e, r, blocked) = (entered.clone(), release.clone(), a.clone());
        let (requests, seen) = start(Arc::new(move |_: &str, body: &Value| {
            if body["id"] == json!(blocked) {
                e.set();
                r.wait(Duration::from_secs(3));
            }
            Ok(Map::new())
        }));
        assert!(requests
            .submit("send", "/v1/send", &json!({"id": a}), tag("blocked"), None)
            .unwrap());
        assert!(entered.wait(Duration::from_secs(2)));
        assert!(requests
            .submit("send", "/v1/send", &json!({"id": b}), tag("other"), None)
            .unwrap());
        assert!(requests
            .submit(
                "register",
                "/v1/register",
                &json!({"sessions": []}),
                (0, "register".into(), "register"),
                None
            )
            .unwrap());
        let mut done: Vec<String> = (0..2).map(|_| next(&seen).0 .1).collect();
        done.sort();
        assert_eq!(done, ["other", "register"]);
        release.set();
        assert_eq!(next(&seen).0 .1, "blocked");
        requests.close();
    }

    #[test]
    fn snapshot_and_fifo_status_order() {
        let (entered, release) = (Arc::new(Flag::new()), Arc::new(Flag::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (e, r, b) = (entered.clone(), release.clone(), bodies.clone());
        let (requests, seen) = start(Arc::new(move |_: &str, body: &Value| {
            if b.lock().unwrap().is_empty() {
                e.set();
                r.wait(Duration::from_secs(3));
            }
            b.lock().unwrap().push(body.clone());
            Ok(Map::new())
        }));
        requests
            .submit(
                "send",
                "/v1/send",
                &json!({"id": "same"}),
                tag("first"),
                None,
            )
            .unwrap();
        assert!(entered.wait(Duration::from_secs(2)));
        let mut body = json!({"id": "same", "state": "held", "nested": {"value": 1}});
        requests
            .submit("status", "/v1/status", &body, tag("held"), None)
            .unwrap();
        body["nested"]["value"] = json!(9);
        requests
            .submit(
                "status",
                "/v1/status",
                &json!({"id": "same", "state": "handed_to_claude"}),
                tag("release"),
                None,
            )
            .unwrap();
        release.set();
        let order: Vec<String> = (0..3).map(|_| next(&seen).0 .1).collect();
        assert_eq!(order, ["first", "held", "release"]);
        assert_eq!(bodies.lock().unwrap()[1]["nested"]["value"], json!(1));
        requests.close();
    }

    #[test]
    fn saturation_and_close_cancel_only_queued_no_retry() {
        let (entered, release) = (Arc::new(Flag::new()), Arc::new(Flag::new()));
        let calls = Arc::new(Mutex::new(0));
        let (e, r, c) = (entered.clone(), release.clone(), calls.clone());
        let (requests, seen) = start(Arc::new(move |_: &str, _: &Value| {
            *c.lock().unwrap() += 1;
            e.set();
            r.wait(Duration::from_secs(3));
            Err(HubError::Os("ambiguous transport failure".into()))
        }));
        requests
            .submit(
                "send",
                "/v1/send",
                &json!({"id": "same"}),
                tag("active"),
                None,
            )
            .unwrap();
        assert!(entered.wait(Duration::from_secs(2)));
        for i in 0..32 {
            assert!(requests
                .submit(
                    "status",
                    "/v1/status",
                    &json!({"id": "same"}),
                    tag(&i.to_string()),
                    None
                )
                .unwrap());
        }
        assert!(!requests
            .submit(
                "status",
                "/v1/status",
                &json!({"id": "same"}),
                tag("rejected"),
                None
            )
            .unwrap());
        let started = std::time::Instant::now();
        requests.close();
        assert!(started.elapsed() < Duration::from_millis(500));
        let cancelled: Vec<_> = (0..32).map(|_| next(&seen)).collect();
        assert!(cancelled.iter().all(|c| c.2 == Some(HubError::Cancelled)));
        assert!(!requests
            .submit(
                "register",
                "/v1/register",
                &json!({}),
                (0, "closed".into(), "register"),
                None
            )
            .unwrap());
        assert_eq!(*calls.lock().unwrap(), 1);
        release.set();
        let (tag, result, error) = next(&seen);
        assert_eq!(tag.1, "active");
        assert!(result.is_none());
        assert!(matches!(error, Some(HubError::Os(_))));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn generation_and_selection_cancellation_prevents_queued_network_calls() {
        let (entered, release) = (Arc::new(Flag::new()), Arc::new(Flag::new()));
        let labels = Arc::new(Mutex::new(Vec::new()));
        let (e, r, l) = (entered.clone(), release.clone(), labels.clone());
        let (requests, seen) = start(Arc::new(move |_: &str, body: &Value| {
            let label = body["label"].as_str().unwrap().to_string();
            l.lock().unwrap().push(label.clone());
            if label == "active" {
                e.set();
                r.wait(Duration::from_secs(3));
            }
            Ok(Map::new())
        }));
        requests
            .submit(
                "send",
                "/v1/send",
                &json!({"id": "same", "label": "active"}),
                (1, "active".into(), "send"),
                None,
            )
            .unwrap();
        assert!(entered.wait(Duration::from_secs(2)));
        for (label, generation, kind) in [
            ("removed", 1, "send"),
            ("new", 2, "send"),
            ("old-status", 1, "status"),
        ] {
            let path = format!("/v1/{kind}");
            requests
                .submit(
                    kind,
                    &path,
                    &json!({"id": "same", "label": label}),
                    (generation, label.into(), kind),
                    None,
                )
                .unwrap();
        }
        assert_eq!(requests.cancel(|tag| tag.1 == "removed"), 1);
        assert_eq!(requests.cancel(|tag| tag.0 == 1), 1);
        assert_eq!(requests.cancel(|tag| tag.0 == 1), 0);
        let mut cancelled: Vec<String> = (0..2)
            .map(|_| {
                let item = next(&seen);
                assert_eq!(item.2, Some(HubError::Cancelled));
                item.0 .1
            })
            .collect();
        cancelled.sort();
        assert_eq!(cancelled, ["old-status", "removed"]);
        release.set();
        let done: Vec<String> = (0..2).map(|_| next(&seen).0 .1).collect();
        assert_eq!(done, ["active", "new"]);
        assert_eq!(*labels.lock().unwrap(), ["active", "new"]);
        requests.close();
        assert!(seen.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn registration_lane_bounded_and_input_validation() {
        let (entered, release) = (Arc::new(Flag::new()), Arc::new(Flag::new()));
        let (e, r) = (entered.clone(), release.clone());
        let (requests, _seen) = start(Arc::new(move |_: &str, _: &Value| {
            e.set();
            r.wait(Duration::from_secs(3));
            Ok(Map::new())
        }));
        let register = |label: &str| (0, label.to_string(), "register");
        requests
            .submit("register", "/v1/register", &json!({}), register("1"), None)
            .unwrap();
        assert!(entered.wait(Duration::from_secs(2)));
        assert!(requests
            .submit("register", "/v1/register", &json!({}), register("2"), None)
            .unwrap());
        assert!(!requests
            .submit("register", "/v1/register", &json!({}), register("3"), None)
            .unwrap());
        assert!(requests
            .submit("send", "/v1/register", &json!({}), tag("4"), None)
            .is_err());
        assert!(requests
            .submit("send", "/v1/send", &json!({}), tag("5"), None)
            .is_err());
        let error = requests
            .submit(
                "send",
                "/v1/send",
                &json!({"id": "a", "text": "a".repeat(65536)}),
                tag("6"),
                None,
            )
            .unwrap_err();
        assert!(error.message.contains("too large"));
        release.set();
        requests.close();
    }
}
