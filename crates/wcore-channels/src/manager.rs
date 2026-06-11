//! `ChannelManager` — drives a registry of `Channel` impls.
//!
//! v0.7.0 2.A.2: each channel runs on its own tokio task that
//! polls `poll_events()` and forwards results to a single broadcast
//! channel the engine + UI subscribe to. Outbound sends go through
//! `send_to(name, msg)` which routes to the channel's send_message.
//!
//! Concurrency model: each channel is held in an `Arc<Mutex<Box<dyn
//! Channel>>>` so the poll task and the send path serialize against
//! the same instance (most platform SDKs aren't `Sync`-on-write).
//! Polling cadence is configurable; default 250ms.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::Channel;
use crate::error::ChannelError;
use crate::event::{ChannelEvent, ConnectionState, MessageReceipt};
use crate::outgoing::OutgoingMessage;

const DEFAULT_POLL_INTERVAL_MS: u64 = 250;
const EVENT_CHANNEL_CAP: usize = 256;

/// Consecutive non-`NotStarted` poll errors tolerated before the poll
/// task treats the channel as disconnected and enters supervised
/// reconnect. Below this, errors back off one tick and retry (the
/// historical behavior) to absorb transient blips without churn.
const RECONNECT_ERROR_THRESHOLD: u32 = 5;
/// First reconnect-attempt backoff. Doubles each failed `start()` up to
/// `RECONNECT_BACKOFF_CAP`.
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Upper bound on reconnect backoff so a permanently broken channel
/// retries at a steady, low rate rather than escalating unbounded.
const RECONNECT_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Driver for a set of `Channel` instances. Build with `new`, register
/// channels with `register`, then call `start_all` to spawn the poll
/// tasks. `subscribe()` returns a tokio broadcast receiver carrying
/// `ChannelEvent`s tagged with the originating channel name.
pub struct ChannelManager {
    channels: HashMap<String, Arc<Mutex<Box<dyn Channel>>>>,
    poll_tasks: HashMap<String, JoinHandle<()>>,
    poll_interval: Duration,
    events_tx: broadcast::Sender<TaggedEvent>,
}

/// One `ChannelEvent` annotated with the channel that produced it.
#[derive(Debug, Clone)]
pub struct TaggedEvent {
    pub channel_name: String,
    pub event: ChannelEvent,
}

impl ChannelManager {
    pub fn new() -> Self {
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        Self {
            channels: HashMap::new(),
            poll_tasks: HashMap::new(),
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
            events_tx,
        }
    }

    /// Override the polling interval. Default 250ms.
    pub fn with_poll_interval(mut self, dur: Duration) -> Self {
        self.poll_interval = dur;
        self
    }

    /// Register a channel. Replaces any existing channel under the
    /// same name (stops the old poll task first).
    pub async fn register(&mut self, ch: Box<dyn Channel>) {
        let name = ch.name().to_string();
        if let Some(handle) = self.poll_tasks.remove(&name) {
            handle.abort();
        }
        self.channels.insert(name, Arc::new(Mutex::new(ch)));
    }

    /// Subscribe to the unified event stream. Late subscribers miss
    /// events emitted before they subscribed (broadcast semantics).
    pub fn subscribe(&self) -> broadcast::Receiver<TaggedEvent> {
        self.events_tx.subscribe()
    }

    /// Start every registered channel and spawn its poll task.
    /// Idempotent — channels already started skip re-start.
    pub async fn start_all(&mut self) -> Result<(), ChannelError> {
        for (name, slot) in self.channels.iter() {
            if self.poll_tasks.contains_key(name) {
                continue;
            }
            {
                let mut guard = slot.lock().await;
                guard.start().await?;
            }
            let task_slot = Arc::clone(slot);
            let task_name = name.clone();
            let task_tx = self.events_tx.clone();
            let interval = self.poll_interval;
            let handle = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // Consecutive non-`NotStarted` poll errors. Reset to 0 on
                // any successful poll. Crossing `RECONNECT_ERROR_THRESHOLD`
                // promotes the channel to supervised reconnect.
                let mut consecutive_errors: u32 = 0;
                loop {
                    ticker.tick().await;
                    let evs = {
                        let mut guard = task_slot.lock().await;
                        match guard.poll_events().await {
                            Ok(v) => {
                                consecutive_errors = 0;
                                v
                            }
                            Err(ChannelError::NotStarted) => break,
                            Err(e) => {
                                consecutive_errors += 1;
                                tracing::warn!(
                                    target: "wcore_channels::manager",
                                    channel = %task_name,
                                    error = %e,
                                    consecutive_errors,
                                    "poll_events errored; backing off one tick"
                                );
                                if consecutive_errors < RECONNECT_ERROR_THRESHOLD {
                                    continue;
                                }
                                // Drop the guard before the reconnect loop so we
                                // don't hold the slot lock across backoff sleeps
                                // (send_to / stop_all must still acquire it).
                                drop(guard);
                                // Supervised reconnect: announce Reconnecting and
                                // retry start() with exponential backoff until it
                                // succeeds. The task is stopped via handle.abort()
                                // (stop_all / register replace), so the sleeps
                                // below double as the abort points.
                                let _ = task_tx.send(TaggedEvent {
                                    channel_name: task_name.clone(),
                                    event: ChannelEvent::ConnectionStateChanged {
                                        state: ConnectionState::Reconnecting,
                                    },
                                });
                                let mut backoff = RECONNECT_BACKOFF_BASE;
                                loop {
                                    tokio::time::sleep(backoff).await;
                                    let start_result = {
                                        let mut guard = task_slot.lock().await;
                                        guard.start().await
                                    };
                                    match start_result {
                                        Ok(()) => {
                                            tracing::info!(
                                                target: "wcore_channels::manager",
                                                channel = %task_name,
                                                "channel reconnected; resuming polling"
                                            );
                                            consecutive_errors = 0;
                                            break;
                                        }
                                        Err(re) => {
                                            backoff = (backoff * 2).min(RECONNECT_BACKOFF_CAP);
                                            tracing::warn!(
                                                target: "wcore_channels::manager",
                                                channel = %task_name,
                                                error = %re,
                                                next_backoff_ms = backoff.as_millis() as u64,
                                                "reconnect start() failed; will retry"
                                            );
                                        }
                                    }
                                }
                                // Reconnected — skip this tick's broadcast and
                                // resume the normal polling cadence.
                                continue;
                            }
                        }
                    };
                    for event in evs {
                        let _ = task_tx.send(TaggedEvent {
                            channel_name: task_name.clone(),
                            event,
                        });
                    }
                }
            });
            self.poll_tasks.insert(name.clone(), handle);
        }
        Ok(())
    }

    /// Stop every registered channel + abort its poll task.
    pub async fn stop_all(&mut self) -> Result<(), ChannelError> {
        let names: Vec<String> = self.channels.keys().cloned().collect();
        for name in names {
            if let Some(handle) = self.poll_tasks.remove(&name) {
                handle.abort();
            }
            if let Some(slot) = self.channels.get(&name) {
                let mut guard = slot.lock().await;
                let _ = guard.stop().await;
            }
        }
        Ok(())
    }

    /// Send a message through a named channel.
    pub async fn send_to(
        &self,
        name: &str,
        msg: OutgoingMessage,
    ) -> Result<MessageReceipt, ChannelError> {
        let slot = self
            .channels
            .get(name)
            .ok_or_else(|| ChannelError::Config(format!("unknown channel: {name}")))?;
        let mut guard = slot.lock().await;

        // Split over-long bodies to the platform cap so a long reply is
        // delivered in pieces rather than rejected+dropped (HIGH-6). When the
        // connector declares no cap (or the body already fits) this is a
        // single send, byte-identical to the pre-chunking path.
        let chunks = match guard.max_message_len() {
            Some(max) if max > 0 => crate::chunk::chunk_message(&msg.text, max),
            _ => vec![msg.text.clone()],
        };
        if chunks.len() <= 1 {
            return guard.send_message(msg).await;
        }

        // Multi-chunk: each piece keeps the conversation + reply target;
        // attachments ride the LAST chunk (so the text precedes the media).
        // Returns the final chunk's receipt.
        let last = chunks.len() - 1;
        let mut receipt: Option<MessageReceipt> = None;
        for (i, chunk) in chunks.into_iter().enumerate() {
            let part = OutgoingMessage {
                conversation_id: msg.conversation_id.clone(),
                text: chunk,
                reply_to: msg.reply_to.clone(),
                attachments: if i == last {
                    msg.attachments.clone()
                } else {
                    Vec::new()
                },
            };
            receipt = Some(guard.send_message(part).await?);
        }
        // INVARIANT: chunks.len() > 1 here, so the loop ran and set `receipt`.
        receipt.ok_or_else(|| ChannelError::Other("chunked send produced no receipt".into()))
    }

    /// Send a transient typing indicator to `conversation_id` on channel
    /// `name`. Best-effort: unknown channel → `Config` error; platforms
    /// without a typing API no-op via the trait default.
    pub async fn send_typing_to(
        &self,
        name: &str,
        conversation_id: &str,
    ) -> Result<(), ChannelError> {
        let slot = self
            .channels
            .get(name)
            .ok_or_else(|| ChannelError::Config(format!("unknown channel: {name}")))?;
        let guard = slot.lock().await;
        guard.send_typing(conversation_id).await
    }

    /// React to `message_id` in `conversation_id` on channel `name` with a
    /// unicode emoji (ack state machine). Unknown channel → `Config` error;
    /// platforms without reactions → `Rejected` via the trait default.
    pub async fn react_on(
        &self,
        name: &str,
        conversation_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), ChannelError> {
        let slot = self
            .channels
            .get(name)
            .ok_or_else(|| ChannelError::Config(format!("unknown channel: {name}")))?;
        let guard = slot.lock().await;
        guard.react(conversation_id, message_id, emoji).await
    }

    /// Fetch an inbound attachment's bytes through the originating channel
    /// `name`, which holds its own credentials and platform media protocol.
    /// Mirrors [`Self::react_on`]. Unknown channel → `Config` error;
    /// connectors without media support → `Rejected` via the trait default.
    ///
    /// NOTE (concurrency): like `react_on`/`send_typing_to`, this holds the
    /// channel's mutex across the download, briefly pausing that one channel's
    /// poll/send while its own just-received media is fetched. The enricher
    /// bounds the call with a timeout so a slow media host can't stall it.
    pub async fn fetch_media_on(
        &self,
        name: &str,
        attachment: &crate::event::Attachment,
    ) -> Result<Vec<u8>, ChannelError> {
        let slot = self
            .channels
            .get(name)
            .ok_or_else(|| ChannelError::Config(format!("unknown channel: {name}")))?;
        let guard = slot.lock().await;
        guard.fetch_media(attachment).await
    }

    /// List names of registered channels, sorted alphabetically.
    pub fn list_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.channels.keys().cloned().collect();
        names.sort();
        names
    }

    /// Route an inbound webhook request to channel `name`'s
    /// [`Channel::ingest_webhook`](crate::Channel::ingest_webhook). The
    /// connector verifies the platform signature, parses the body, and
    /// enqueues any resulting event(s) for the next `poll_events()` (which
    /// the inbound subscriber drains). The returned
    /// [`WebhookResponse`](crate::webhook::WebhookResponse) is what the host
    /// writes back to the platform. Unknown channel → `Config` error (the
    /// host maps it to a 404). Mirrors [`Self::send_to`] for inbound.
    pub async fn ingest_webhook(
        &self,
        name: &str,
        req: &crate::webhook::WebhookRequest,
    ) -> Result<crate::webhook::WebhookResponse, ChannelError> {
        let slot = self
            .channels
            .get(name)
            .ok_or_else(|| ChannelError::Config(format!("unknown channel: {name}")))?;
        let guard = slot.lock().await;
        guard.ingest_webhook(req).await
    }
}

impl Default for ChannelManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::IncomingMessage;
    use crate::mock::MockChannel;
    use async_trait::async_trait;
    use std::time::Duration;

    /// Test-only channel whose `poll_events` errors until the manager
    /// re-`start()`s it (the reconnect primitive), after which it recovers
    /// and delivers a single injected message. Models a channel whose
    /// polling breaks until supervised reconnect heals it.
    struct FlakyChannel {
        name: String,
        /// True once the channel has been started at least once.
        started_once: bool,
        /// True after a second `start()` (the manager's reconnect).
        recovered: bool,
        /// True once the recovery message has been delivered.
        delivered: bool,
    }

    impl FlakyChannel {
        fn new(name: impl Into<String>) -> Self {
            Self {
                name: name.into(),
                started_once: false,
                recovered: false,
                delivered: false,
            }
        }
    }

    #[async_trait]
    impl Channel for FlakyChannel {
        fn name(&self) -> &str {
            &self.name
        }

        fn platform(&self) -> &str {
            "flaky"
        }

        async fn start(&mut self) -> Result<(), ChannelError> {
            // First start() = initial connect. Any later start() is the
            // manager's reconnect attempt, which heals the channel.
            if self.started_once {
                self.recovered = true;
            }
            self.started_once = true;
            Ok(())
        }

        async fn stop(&mut self) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn poll_events(&mut self) -> Result<Vec<ChannelEvent>, ChannelError> {
            if self.recovered {
                if !self.delivered {
                    self.delivered = true;
                    return Ok(vec![ChannelEvent::MessageReceived {
                        msg: IncomingMessage::new("flaky-1", "c1", "alice", "back online", 0),
                    }]);
                }
                return Ok(Vec::new());
            }
            // Still in the failing window: error until reconnect heals us.
            Err(ChannelError::Transport("simulated poll failure".into()))
        }

        async fn send_message(
            &mut self,
            msg: OutgoingMessage,
        ) -> Result<MessageReceipt, ChannelError> {
            Ok(MessageReceipt {
                id: "flaky-out".into(),
                conversation_id: msg.conversation_id,
                ts_secs: 0,
            })
        }

        fn config_schema(&self) -> &str {
            r#"{"name": "string", "platform": "flaky"}"#
        }
    }

    /// Test-only channel with a small `max_message_len` that records every
    /// `send_message` into a shared log, so a test can assert how `send_to`
    /// chunked an over-long body.
    struct CappedChannel {
        name: String,
        cap: usize,
        sent: std::sync::Arc<tokio::sync::Mutex<Vec<OutgoingMessage>>>,
    }

    impl CappedChannel {
        fn new(
            name: &str,
            cap: usize,
        ) -> (
            Self,
            std::sync::Arc<tokio::sync::Mutex<Vec<OutgoingMessage>>>,
        ) {
            let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
            (
                Self {
                    name: name.into(),
                    cap,
                    sent: std::sync::Arc::clone(&sent),
                },
                sent,
            )
        }
    }

    #[async_trait]
    impl Channel for CappedChannel {
        fn name(&self) -> &str {
            &self.name
        }
        fn platform(&self) -> &str {
            "capped"
        }
        async fn start(&mut self) -> Result<(), ChannelError> {
            Ok(())
        }
        async fn stop(&mut self) -> Result<(), ChannelError> {
            Ok(())
        }
        async fn poll_events(&mut self) -> Result<Vec<ChannelEvent>, ChannelError> {
            Ok(Vec::new())
        }
        async fn send_message(
            &mut self,
            msg: OutgoingMessage,
        ) -> Result<MessageReceipt, ChannelError> {
            let idx = {
                let mut log = self.sent.lock().await;
                log.push(msg.clone());
                log.len() - 1
            };
            Ok(MessageReceipt {
                id: format!("capped-out-{idx}"),
                conversation_id: msg.conversation_id,
                ts_secs: 0,
            })
        }
        fn config_schema(&self) -> &str {
            r#"{"name":"string","platform":"capped"}"#
        }
        fn max_message_len(&self) -> Option<usize> {
            Some(self.cap)
        }
    }

    #[tokio::test]
    async fn send_to_chunks_overlong_body_to_the_cap() {
        let (ch, sent) = CappedChannel::new("capped", 10);
        let mut mgr = ChannelManager::new();
        mgr.register(Box::new(ch)).await;

        // 25 chars, no break points → hard-split into 10/10/5.
        let body = "abcdefghijklmnopqrstuvwxy".to_string();
        let receipt = mgr
            .send_to(
                "capped",
                OutgoingMessage {
                    conversation_id: "c1".into(),
                    text: body.clone(),
                    reply_to: Some("t1".into()),
                    attachments: vec!["file://a".into()],
                },
            )
            .await
            .expect("send_to");

        let log = sent.lock().await;
        assert_eq!(log.len(), 3, "25 chars at cap 10 → 3 sends");
        assert!(
            log.iter().all(|m| m.text.chars().count() <= 10),
            "every chunk within the cap"
        );
        assert_eq!(
            log.iter().map(|m| m.text.clone()).collect::<String>(),
            body,
            "lossless reassembly across chunks"
        );
        // reply_to carried on every chunk; attachments only on the last.
        assert!(log.iter().all(|m| m.reply_to.as_deref() == Some("t1")));
        assert!(log[0].attachments.is_empty());
        assert!(log[1].attachments.is_empty());
        assert_eq!(log[2].attachments, vec!["file://a".to_string()]);
        // Receipt is the final chunk's.
        assert_eq!(receipt.id, "capped-out-2");
    }

    #[tokio::test]
    async fn send_to_does_not_chunk_when_within_cap() {
        let (ch, sent) = CappedChannel::new("capped", 100);
        let mut mgr = ChannelManager::new();
        mgr.register(Box::new(ch)).await;
        mgr.send_to(
            "capped",
            OutgoingMessage {
                conversation_id: "c1".into(),
                text: "short".into(),
                reply_to: None,
                attachments: Vec::new(),
            },
        )
        .await
        .expect("send_to");
        assert_eq!(sent.lock().await.len(), 1, "a fitting body is one send");
    }

    #[tokio::test]
    async fn register_and_list() {
        let mut mgr = ChannelManager::new();
        mgr.register(Box::new(MockChannel::new("alpha"))).await;
        mgr.register(Box::new(MockChannel::new("beta"))).await;
        assert_eq!(
            mgr.list_names(),
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    #[tokio::test]
    async fn start_all_emits_connection_state_changes() {
        let mut mgr = ChannelManager::new().with_poll_interval(Duration::from_millis(20));
        let mut rx = mgr.subscribe();
        mgr.register(Box::new(MockChannel::new("alpha"))).await;
        mgr.start_all().await.unwrap();

        // Each MockChannel emits a Connected event on start().
        let tagged = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("event arrived")
            .expect("ok");
        assert_eq!(tagged.channel_name, "alpha");
        assert!(matches!(
            tagged.event,
            ChannelEvent::ConnectionStateChanged { .. }
        ));
        mgr.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn send_to_unknown_channel_errors() {
        let mgr = ChannelManager::new();
        let err = mgr
            .send_to("missing", OutgoingMessage::text("c1", "x"))
            .await
            .expect_err("expected unknown-channel error");
        assert!(matches!(err, ChannelError::Config(_)));
    }

    #[tokio::test]
    async fn send_to_registered_channel_routes() {
        let mut mgr = ChannelManager::new().with_poll_interval(Duration::from_millis(20));
        mgr.register(Box::new(MockChannel::new("alpha"))).await;
        mgr.start_all().await.unwrap();
        // Drain initial state-change event.
        let rx = mgr.subscribe();

        let receipt = mgr
            .send_to("alpha", OutgoingMessage::text("c1", "hello"))
            .await
            .unwrap();
        assert!(!receipt.id.is_empty());
        let _ = rx; // suppress unused
        mgr.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn persistent_poll_failure_triggers_supervised_reconnect() {
        // Fail enough polls to cross the threshold, then recover on the
        // manager's reconnect start(). Assert a Reconnecting state is
        // broadcast and the channel resumes delivering messages.
        let mut mgr = ChannelManager::new().with_poll_interval(Duration::from_millis(5));
        let mut rx = mgr.subscribe();
        mgr.register(Box::new(FlakyChannel::new("flaky"))).await;
        mgr.start_all().await.unwrap();

        // Reconnect backoff base is 1s; allow margin for ticks + delivery.
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        let mut saw_reconnecting = false;
        let mut saw_recovery_msg = false;
        while std::time::Instant::now() < deadline && !(saw_reconnecting && saw_recovery_msg) {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(tagged)) => {
                    assert_eq!(tagged.channel_name, "flaky");
                    match tagged.event {
                        ChannelEvent::ConnectionStateChanged {
                            state: ConnectionState::Reconnecting,
                        } => saw_reconnecting = true,
                        ChannelEvent::MessageReceived { ref msg } if msg.text == "back online" => {
                            saw_recovery_msg = true;
                        }
                        _ => {}
                    }
                }
                _ => continue,
            }
        }
        assert!(
            saw_reconnecting,
            "expected a Reconnecting ConnectionStateChanged broadcast"
        );
        assert!(
            saw_recovery_msg,
            "expected the channel to resume delivering messages after reconnect"
        );
        mgr.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn injected_inbound_reaches_subscriber() {
        let mut mgr = ChannelManager::new().with_poll_interval(Duration::from_millis(15));
        let mut rx = mgr.subscribe();
        let mut ch = MockChannel::new("alpha");
        ch.inject_text("c1", "alice", "hi");
        mgr.register(Box::new(ch)).await;
        mgr.start_all().await.unwrap();

        // Loop until we see the MessageReceived (skip state-change).
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut got_msg = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(tagged)) => {
                    if matches!(tagged.event, ChannelEvent::MessageReceived { .. }) {
                        got_msg = true;
                        break;
                    }
                }
                _ => continue,
            }
        }
        assert!(
            got_msg,
            "expected to receive an injected MessageReceived event"
        );
        mgr.stop_all().await.unwrap();
    }
}
