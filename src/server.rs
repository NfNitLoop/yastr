pub(crate) mod nip95;
pub(crate) mod immutable;

use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{
        ws::{self, WebSocket},
        ConnectInfo, State, WebSocketUpgrade,
    }, response::IntoResponse, routing::get, Json
};
use serde::Serialize;

use crate::db::DB;
use futures::{
    stream::{SplitSink, StreamExt},
    SinkExt as _,
};
use nostr::{Kind, RelayMessage};
use tokio::sync::Mutex;
use tracing::{debug, trace, warn};

pub fn router(db_pool: DB) -> axum::Router {
    axum::Router::new()
        .route("/", get(get_root))
        .nest("/files", nip95::router())
        .with_state(db_pool)
}

pub async fn get_root(
    ws: Option<WebSocketUpgrade>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(db): State<DB>,
) -> impl IntoResponse {
    debug!("Connection from: {addr}");

    if let Some(ws) = ws {
        return ws.on_upgrade(move |socket| handle_nostr_client(socket, db));
    }

    Json(RelayInfo::default()).into_response()
}

/// NIP-11: Serve a Relay Info Document
/// See: <https://github.com/nostr-protocol/nips/blob/master/11.md>
#[derive(Debug, Serialize)]
struct RelayInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    supported_nips: Vec<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    software: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    limitation: Option<Limitation>,
}

impl Default for RelayInfo {
    fn default() -> Self {
        Self {
            name: Some("Yastr Relay".into()),
            software: Some("https://github.com/NfNitLoop/yastr".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            supported_nips: vec![
                1,
                // 45 // TODO: COUNT.
            ],
            limitation: Some(Limitation::default()),
        }
    }
}

/// NIP-11 "limitation" information.
#[derive(Debug, Serialize)]
struct Limitation {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_message_length: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    max_subscriptions: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    max_filters: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    max_limit: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    max_subid_length: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    max_event_tags: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    max_content_length: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    min_pow_difficulty: Option<u8>,

    #[serde(skip_serializing_if = "Option::is_none")]
    auth_required: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    payment_required: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    restricted_writes: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    created_at_lower_limit: Option<u64>,

    /// Examples seem to be small. I assume this is the amount of seconds into the future we'll accept messages for?
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at_upper_limit: Option<u64>,
}

impl Default for Limitation {
    fn default() -> Self {
        Self {
            // ... until we implement more?
            max_filters: Some(1),

            // 128k ought to be enough for anybody. ;)
            max_message_length: Some(128 * 1024),

            // A core feature of Yastr is that it's friends-only by default.
            restricted_writes: Some(true),

            max_subscriptions: None,
            max_limit: None,
            max_subid_length: None,
            max_event_tags: None,
            max_content_length: None,
            min_pow_difficulty: None,
            auth_required: None,
            payment_required: None,
            created_at_lower_limit: None,
            created_at_upper_limit: None,
        }
    }
}

async fn handle_nostr_client(socket: WebSocket, db: DB) {
    debug!("Opened web socket");
    let (sender, mut receiver) = socket.split();

    // TODO: Could pass this in from the handler, w/ connection info, etc.
    let conn = Connection::new(sender, db);

    while let Some(Ok(msg)) = receiver.next().await {
        trace!("Got message: {msg:#?}");
        match msg {
            ws::Message::Binary(_) => {
                debug!("Got unsupported binary message. TODO: Return NOTICE error.");
                continue;
            }
            ws::Message::Ping(_) | ws::Message::Pong(_) => {
                // do nothing for ping/pong. Axum will auto-respond to pings. (Say docs)
            }
            ws::Message::Text(text) => {
                let conn = conn.clone();
                tokio::spawn(async move { conn.got_message(text).await });
            }
            ws::Message::Close(_) => {
                break;
            }
        }
    }
    debug!("Closing web socket");
}

type WSSender = SplitSink<WebSocket, ws::Message>;

/// State for a nostr connection.
struct Connection {
    // subscriptions: HashMap<SubscriptionID, Subscription>,
    sender: Arc<Mutex<WSSender>>,
    db: DB,
}

impl Connection {
    fn new(sender: WSSender, db: DB) -> Arc<Self> {
        Self {
            sender: Arc::new(Mutex::new(sender)),
            db,
        }
        .into()
    }

    async fn got_message(self: &Arc<Self>, text: String) {
        let msg: nostr::ClientMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                let msg = format!("error: Message from client was invalid: {e:#?}");
                warn!("{msg}");
                self.notice(msg);
                return;
            }
        };

        trace!("got message: {msg:?}");

        use nostr::ClientMessage::*;
        match msg {
            Auth(_) | Count { .. } | NegClose { .. } | NegOpen { .. } | NegMsg { .. } => {
                debug!("Unsupported message");
            }
            Event(event) => {
                let conn = Arc::clone(self);
                tokio::spawn(async move { conn.save_event(event).await });
            }
            Req {
                subscription_id,
                filters,
            } => {
                let conn = Arc::clone(self);
                tokio::spawn(async move {
                    conn.handle_search(subscription_id, filters).await;
                });
            }
            Close(sub_id) => self.close_sub(sub_id),
        }
    }

    fn close_sub(&self, _sub_id: nostr::SubscriptionId) {
        // TODO: When we implement long-running subscriptions,
        // we'll need to process close messages to end them.
        // For now, though, subscriptions only exist to answer a REQ
        // for saved items.
    }

    /// Handles REQ (search) requests.
    async fn handle_search(
        self: &Arc<Self>,
        subscription_id: nostr::SubscriptionId,
        filters: Vec<nostr::Filter>,
    ) {
        // We must always close the send EOSE and CLOSE events.
        // So, we do that below, and delegate the rest to the inner function.
        self.handle_search_inner(&subscription_id, filters).await;

        let res = self
            .send(&RelayMessage::EndOfStoredEvents(subscription_id.clone()))
            .await;
        if res.is_err() {
            return;
        }

        let _ = self
            .send(&RelayMessage::Closed {
                subscription_id,
                message: "TODO: implement streaming".into(),
            })
            .await;
    }

    /// Handle everything but the EOSE and CLOSE events.
    async fn handle_search_inner(
        self: &Arc<Self>,
        subscription_id: &nostr::SubscriptionId,
        filters: Vec<nostr::Filter>,
    ) {
        if filters.len() > 1 {
            debug!("got {} filters from client, unsupported.", filters.len());
            self.notice(
                "TODO: support multiple filters. Until then, please use separate subscriptions.",
            );
            return;
        }

        let mut events = self.db.search(filters);

        while let Some(event) = events.recv().await {
            let event = match event {
                Err(err) => {
                    debug!("Database error: {err:?}");
                    return;
                }
                Ok(e) => e,
            };

            let res = self
                .send(&RelayMessage::event(subscription_id.clone(), event))
                .await;
            if res.is_err() {
                trace!("Couldn't send event. Client closed connection?")
            }
        }
    }

    async fn save_event(self: &Arc<Self>, event: Box<nostr::Event>) {
        // note: MUST send an OK(true/false) response to every event we get.

        if let Err(message) = self.check_allowed(&event).await {
            self.send_spawn(RelayMessage::ok(event.id, false, message));
            return;
        }

        let response = match self.db.save_event(&event).await {
            Ok(_) => RelayMessage::ok(event.id, true, ""),
            Err(sqlx::Error::Database(err))
                if err.kind() == sqlx::error::ErrorKind::UniqueViolation =>
            {
                debug!("database error: {err:#?}");
                RelayMessage::ok(event.id, true, "duplicate: duplicate event")
            }
            Err(err) => RelayMessage::ok(event.id, false, err.to_string()),
        };
        let res = self.send(&response).await;
        if res.is_err() {
            // This is probably always going to be because we lost a connection:
            trace!("save_event couldn't send response: {res:?}")
        }
    }

    /// return an error message if the event isn't allowed.
    async fn check_allowed(&self, event: &nostr::Event) -> Result<(), String> {
        // Check in optimized order, cheapest checks first.

        // TODO: Allow a server to configure what types it supports.
        let opts = ServerOptions::default();

        let kind = event.kind();
        let unsupported_type = kind.is_ephemeral()
            || kind.is_job_request()
            || kind.is_job_result()
            || (matches!(kind, Kind::EncryptedDirectMessage) && !opts.allow_encrypted_messages);
        if unsupported_type {
            return Err(format!("blocked: Unsupported event kind: {}", event.kind));
        }

        if event.verify().is_err() {
            return Err(format!("invalid: Invalid signature"));
        }

        // TODO: Cache
        let user_allowed = self
            .db
            .user_allowed(&event.pubkey)
            .await
            .map_err(|e| format!("error: {e}"))?;

        if user_allowed {
            trace!("Granted access based on pubkey: {}", event.pubkey);
            return Ok(());
        }

        // Allow events that are referred to by existing users' events:
        let has_ref = self
            .db
            .referred_event(&event.id)
            .await
            .map_err(|e| format!("error: {e}"))?;

        if has_ref {
            trace!("Granted access for referred event: {}", event.id);
            return Ok(());
        }

        if matches!(event.kind, Kind::Metadata) {
            let has_ref = self
                .db
                .referred_pubkey(&event.pubkey)
                .await
                .map_err(|e| format!("error: {e}"))?;
            if has_ref {
                trace!(
                    "Granted access for referred pubkey profile (kind 0) {}",
                    event.pubkey
                );
                return Ok(());
            }
        }

        // Default error message if none of the above exceptions granted access:
        Err(format!(
            "blocked: Access not granted for pubkey {}",
            event.pubkey
        ))
    }

    async fn send(&self, msg: &RelayMessage) -> crate::result::Result<(), ConnectionError> {
        let json = serde_json::to_string(&msg)?;
        let ws_msg = ws::Message::Text(json);
        let mut sender = self.sender.lock().await;
        trace!("sending message: {msg:?}");
        let res = sender.send(ws_msg).await;
        if let Err(err) = &res {
            debug!("Error sending message: {msg:?}. Error: {err}");
        }
        res?;
        Ok(())
    }

    /// Send and don't wait to check errors.
    fn send_spawn(self: &Arc<Self>, msg: RelayMessage) {
        let conn = Arc::clone(self);
        tokio::spawn(async move {
            let res = conn.send(&msg).await;
            if !res.is_ok() {
                debug!("Error sending message: {msg:?}");
            }
        });
    }

    fn notice(self: &Arc<Self>, msg: impl Into<String>) {
        self.send_spawn(RelayMessage::notice(msg))
    }
}

#[derive(Debug)]
pub(crate) struct ServerOptions {
    allow_encrypted_messages: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            // Encrypted messages don't have forward secrecy and could be compromised by a later key leak.
            allow_encrypted_messages: false,
            // TODO: check event.serialized_size()
            // let max_event_bytes = 128 * 1024; // 128 KiB
            // TODO: disallow events in the future.
            // let max_clock_drift_secs = 3;
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ConnectionError {
    #[error("JSON (de)serialization error")]
    Json(#[from] serde_json::Error),

    #[error("Error communicating with web socket")]
    Axum(#[from] axum::Error),
}

// Clients might send big subscription IDs? Don't copy them.
#[derive(Debug, PartialEq)]
struct SubscriptionID {
    id: Arc<String>,
}
