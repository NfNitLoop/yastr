//! Non-standard extras to support nip95 files.
//!

use std::collections::VecDeque;

use axum::{
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use nostr::event::{EventId, Tag, TagKind};

use futures::{stream, TryStream};
use tracing::warn;

use crate::db::DB;

/// Given an event_id, load the kind 1065 event to get info about it.
///
/// For now I'm being lazy and just spitting out the event JSON, but this could
/// give HTML or JSON depending on the "accept" headers we get.
pub async fn info(State(db): State<DB>, Path(event_id): Path<String>) -> Result<Response, String> {
    let id = nostr::EventId::parse(event_id).map_err(|e| format!("error: {e:?}"))?;
    let event = db
        .get_event(id)
        .await
        .map_err(|e| format!("error: {e:?}"))?;

    let Some(event) = event else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    if event.kind.as_u32() != 1065 {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    // If we wouldn't accept this event, we shouldn't serve it:
    // (Can happen if we got stricter w/ rules after accepting old events.)
    FileMeta::filter_accept(&event)?;

    Ok(Json(event).into_response())
}

/// Allow downloading the file with simple HTTP.
/// TODO: Support range requests: https://developer.mozilla.org/en-US/docs/Web/HTTP/Range_requests
pub async fn get_file(
    State(db): State<DB>,
    Path((event_id, req_file_name)): Path<(String, String)>,
) -> Result<Response, String> {
    let id = nostr::EventId::parse(event_id).map_err(|e| format!("error: {e:?}"))?;
    let event = db
        .get_event(id)
        .await
        .map_err(|e| format!("error: {e:?}"))?;

    let Some(event) = event else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    if event.kind.as_u32() != 1065 {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    FileMeta::filter_accept(&event)?;

    let meta = FileMeta { event: &event };

    let file_name = meta.file_name()?;
    // Note: Most NIP95 files *don't* have a file name:
    if let Some(file_name) = file_name {
        if file_name != req_file_name {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }

        // TODO: Set mime type headers depending on the file name?
        // Or the mime type in the payload. But this could be dangerous?
    }

    let event_ids = meta
        .event_ids()
        .into_iter()
        .map(|it| it.to_owned())
        .collect::<Vec<_>>();

    let total_size = meta.size()?;

    // TODO: mime type, content-length

    let stream = stream_bytes(db, event_ids);

    Ok(Body::from_stream(stream).into_response())
}

/// A wrapper around a [`nostr::Event`] with extra functionality to support Kind 1065 file metadata events.
pub(crate) struct FileMeta<'a> {
    event: &'a nostr::Event,
}

impl<'a> From<&'a nostr::Event> for FileMeta<'a> {
    fn from(event: &'a nostr::Event) -> Self {
        Self { event }
    }
}

impl<'a> FileMeta<'a> {
    /// Returns an error string if we should reject this message.
    fn filter_accept(event: &nostr::Event) -> Result<(), String> {
        if event.kind.as_u32() != 1065 {
            // We're not checking this kind of event.
            return Ok(());
        }
        let meta = FileMeta::from(event);
        let event_ids = meta.event_ids();
        if event_ids.is_empty() {
            return Err(String::from(
                "Event must have at least one event ID reference.",
            ));
        }

        meta.size()?;

        if event_ids.len() > 1 {
            // Multi-part messages MUST include a blockSize, so that clients can know how to
            // calculate byte offsets before fetching every individual message.
            meta.block_size()?;

            // Going to enforce some other, stricter requirements for multi-part messages as well:

            // If I'm going to store big(er) files for you, at least tell me its name.
            let file_name = meta.file_name()?;
            let Some(file_name) = file_name else {
                return Err(String::from("a fileName is required."));
            };
            // TODO: Better path checking here.  first pass:
            if file_name.contains("/") || file_name.contains("..") || file_name.contains("\\") {
                return Err(String::from("invalid file name"));
            }
        }

        Ok(())
    }
}

impl<'a> FileMeta<'a> {
    pub fn event_ids(&self) -> Vec<&EventId> {
        self.event
            .tags()
            .iter()
            .filter_map(|t| {
                let Tag::Event { event_id, .. } = t else {
                    return None;
                };
                Some(event_id)
            })
            .collect()
    }

    pub fn is_multi_part(&self) -> bool {
        self.event_ids().len() > 1
    }

    pub fn file_name(&self) -> Result<Option<String>, String> {
        // TODO: Ugh, all tags are custom until they're not.  This seems like a fragile API. If they decide to ever add an enum for "fileName" this will fail.
        let tags = self
            .event
            .tags()
            .iter()
            .flat_map(|tag| match tag {
                Tag::Generic(TagKind::Custom(tag_name), values)
                    if tag_name == "fileName" && values.len() == 1 =>
                {
                    Some(values[0].clone())
                }
                _ => None,
            })
            .take(2)
            .collect::<Vec<_>>();

        if tags.len() > 1 {
            return Err(format!("Multiple fileName tags found: {tags:?}"));
        }

        return Ok(tags.into_iter().next());
    }

    // Get the "size" tag:
    pub fn size(&self) -> Result<u64, String> {
        let sizes = self.event.tags().iter().filter_map(|tag| {
            let Tag::Generic(TagKind::Size, values) = tag else {
                return None;
            };
            values.into_iter().next()
        });

        let Some(size) = sizes.into_iter().next() else {
            warn!("missing size from: {:#?}", self.event);
            return Err(format!("Missing required size."));
        };

        let size = size
            .parse::<u64>()
            .map_err(|e| format!("Error parsing size: {e}"))?;

        Ok(size)

        // let tag_name = "size";
        // let values = self.require_custom_tag("size")?;
        // let Some(value) = values.into_iter().next() else {
        //     return Err(format!("No value for tag {tag_name}"));
        // };
        // let value = value
        //     .parse::<u64>()
        //     .map_err(|e| format!("Error parsing value for tag {tag_name}: {e:?}"))?;
        // Ok(value)
    }

    pub fn block_size(&self) -> Result<Option<u64>, String> {
        let tag_name = "blockSize";
        let Some(values) = self.custom_tag(tag_name)? else {
            if self.is_multi_part() {
                return Err(format!(
                    "A multi-part message requires a blockSize argument."
                ));
            }
            return Ok(None);
        };
        let Some(size_text) = values.into_iter().next() else {
            return Err(String::from("Missing value for {tag_name}"));
        };
        let size = match size_text.parse::<u64>() {
            Ok(s) => s,
            Err(e) => return Err(format!("Invalid value for {tag_name}: {e}")),
        };

        Ok(Some(size))
    }
}

/// Private/helper functions
// TODO: These are pretty generic to "event", maybe make a helper trait for that and move these there?
impl<'a> FileMeta<'a> {
    /// Return the values from each tag matching a given name.
    fn custom_tags(&self, tag_name: &str) -> Vec<&Vec<String>> {
        self.event
            .tags()
            .iter()
            .filter_map(|tag| {
                let Tag::Generic(TagKind::Custom(custom_name), values) = tag else {
                    return None;
                };
                if custom_name != tag_name {
                    return None;
                };
                Some(values)
            })
            .collect()
    }

    /// Like [`Self::custom_tags`], but returns an errror if more than one is found.
    fn custom_tag(&self, tag_name: &str) -> Result<Option<&Vec<String>>, String> {
        let tags = self.custom_tags(tag_name);
        if tags.len() > 1 {
            return Err(format!(
                "Expected 1 tag named {tag_name} but found {}",
                tags.len()
            ));
        }

        let values = tags.into_iter().next();
        Ok(values)
    }

    /// Like [`Self::custom_tag`], but return an error if the tag is missing.
    fn require_custom_tag(&self, tag_name: &str) -> Result<&Vec<String>, String> {
        let Some(values) = self.custom_tag(tag_name)? else {
            return Err(format!("Required tag {tag_name} is missing"));
        };
        Ok(values)
    }
}

fn stream_bytes(db: DB, event_ids: Vec<EventId>) -> impl TryStream<Ok = Vec<u8>, Error = String> {
    struct State {
        event_ids: VecDeque<EventId>,
        db: DB,
    }

    let state = State {
        event_ids: VecDeque::from(event_ids),
        db,
    };

    stream::try_unfold(state, |mut state| async move {
        let Some(event_id) = state.event_ids.pop_front() else {
            return Ok(None);
        };

        let event = state
            .db
            .get_event(event_id)
            .await
            .map_err(|e| format!("error: {e:?}"))?;
        let Some(event) = event else {
            return Err(format!("no such event: {event_id}"));
        };

        use base64::prelude::BASE64_STANDARD as b64;
        use base64::Engine as _;
        let bytes = b64
            .decode(event.content.as_str())
            .map_err(|e| format!("Error decoding base64 content: {e:?}"))?;

        Ok(Some((bytes, state)))
    })
}

// TODO: maybe use stream.unfold to make a stream: https://docs.rs/futures/latest/futures/stream/fn.unfold.html
// Then you can make a Body from it: https://docs.rs/axum/latest/axum/body/struct.Body.html#method.from_streamq
