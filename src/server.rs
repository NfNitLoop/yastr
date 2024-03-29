use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    extract::{
        ws::{self, WebSocket},
        ConnectInfo, State, WebSocketUpgrade,
    },
    http::{HeaderMap, Response},
    response::IntoResponse,
    Json,
};
use axum_extra::{headers, TypedHeader};
use serde::Serialize;

use crate::db::DB;
use futures::{
    stream::{SplitSink, StreamExt},
    SinkExt as _,
};
use nostr::RelayMessage;
use tokio::sync::Mutex;
use tracing::{debug, trace};

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
    name: Option<String>,
    supported_nips: Vec<u32>,
    software: Option<String>,
    version: Option<String>,
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
    subscriptions: HashMap<SubscriptionID, Subscription>,
    sender: Arc<Mutex<WSSender>>,
    db: DB,
}

impl Connection {
    fn new(sender: WSSender, db: DB) -> Arc<Self> {
        Self {
            subscriptions: HashMap::new(),
            sender: Arc::new(Mutex::new(sender)),
            db,
        }
        .into()
    }

    async fn got_message(self: &Arc<Self>, text: String) {
        let msg: nostr::ClientMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => {
                debug!("Message from client was invalid.");
                self.notice("Your client sent an invalid message.");
                return;
            }
        };

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

        // TODO: Check allowed pubkeys. (probably cached?)

        let kind = event.kind();
        if kind.is_ephemeral() || kind.is_job_request() || kind.is_job_result() {
            self.send_spawn(RelayMessage::ok(
                event.id,
                false,
                format!("error: Unsupported event kind: {}", event.kind),
            ));
            return;
        }

        if event.verify().is_err() {
            self.send_spawn(RelayMessage::ok(event.id, false, "Invalid signature"));
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

// TODO: impl DROP.
struct Subscription {
    id: SubscriptionID,
    sender: Arc<Mutex<SplitSink<WebSocket, ws::Message>>>,
}
