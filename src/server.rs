use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    extract::{
        ws::{self, WebSocket},
        ConnectInfo, State, WebSocketUpgrade,
    },
    response::IntoResponse,
};

use crate::db::DB;
use futures::{
    stream::{SplitSink, StreamExt},
    SinkExt as _,
};
use nostr::RelayMessage;
use tokio::sync::Mutex;
use tracing::{debug, trace};

pub async fn get_root(
    ws: WebSocketUpgrade,
    // user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(db): State<DB>,
) -> impl IntoResponse {
    // TODO: debug
    debug!("Connection from: {addr}");

    ws.on_upgrade(move |socket| handle_nostr_client(socket, db))
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
            Err(e) => {
                debug!("Message from client was invalid.");
                debug!("TODO: notify the client.");
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
                self.handle_request(subscription_id, filters);
            }
            Close(sub_id) => self.close_sub(),
        }
    }

    fn close_sub(&self) {
        todo!()
    }

    fn handle_request(
        self: &Arc<Self>,
        subscription_id: nostr::SubscriptionId,
        filters: Vec<nostr::Filter>,
    ) {
        self.send_spawn(nostr::RelayMessage::Closed {
            subscription_id,
            message: "TODO: implement requests".into(),
        });
    }

    // note: MUST send an OK(true/false) response to every event we get.
    async fn save_event(self: &Arc<Self>, event: Box<nostr::Event>) {
        // TODO: Check allowed pubkeys.

        let kind = event.kind();
        if kind.is_ephemeral() || kind.is_job_request() || kind.is_job_result() {
            self.send_spawn(RelayMessage::ok(
                event.id,
                false,
                "error: Unsuppored event kind.",
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

    async fn send(&self, msg: &nostr::RelayMessage) -> crate::result::Result<(), ConnectionError> {
        let json = serde_json::to_string(&msg)?;
        let ws_msg = ws::Message::Text(json);
        let mut sender = self.sender.lock().await;
        trace!("sending message: {msg:?}");
        sender.send(ws_msg).await?;
        Ok(())
    }

    /// Send and don't wait to check errors.
    fn send_spawn(self: &Arc<Self>, msg: nostr::RelayMessage) {
        let conn = Arc::clone(self);
        tokio::spawn(async move {
            let res = conn.send(&msg).await;
            if !res.is_ok() {
                debug!("Error sending message: {msg:?}");
            }
        });
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
