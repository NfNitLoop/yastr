use std::{io, pin::Pin, task::{Context, Poll}};

use axum_range::{AsyncSeekStart, RangeBody};
use futures::{Future, FutureExt};
use nostr::event::{Event, EventId};
use tokio::io::AsyncRead;
use tracing::trace;

use crate::db::DB;



pub struct MultipartRangeInit {
    pub event_ids: Vec<EventId>,
    pub db: DB,
    pub total_size: u64,
    pub block_size: u64,
}

pub struct MultipartRange {
    init: MultipartRangeInit,
    offset: u64,
    state: State,
}

impl MultipartRange {
    pub fn new(init: MultipartRangeInit) -> Self {
        Self {
            init,
            offset: 0,
            state: State::Empty,
        }
    }
}

/// Helper functions.
impl MultipartRange {
    /// What block should we load to satisfy the current offset?
    fn block_to_load(&self) -> EventId {
        let block_size = self.init.block_size;
        let index = (self.offset / block_size) as usize;
        self.init.event_ids[index]
    }

    fn offset_in_block(&self) -> u64 {
        self.offset % self.init.block_size
    }
}

enum State {
    Empty,
    AwaitingLoad(Pin<Box<dyn Send + Future<Output = Result<Option<Event>, sqlx::Error>>>>),
    HaveBytes(Vec<u8>),
}

impl AsyncRead for MultipartRange {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use State::*;

        // TODO: Figure out why I have to move this here.
        // If I put it at [2], then I get an error that I can't borrow self immutably, because it's borrowed mutably (at [3]).
        // But ... [1] doesn't have that problem. Why?
        let block_offset = self.offset_in_block() as usize;
        
        match &mut self.state { // TODO: [3]
            Empty => {
                if self.offset >= self.init.total_size {
                    // We're done reading:
                    return Poll::Ready(Ok(()));
                }

                let id = self.block_to_load();
                let db = self.init.db.clone();
                let fut = async move {
                    db.get_event(id).await
                };
                self.state = AwaitingLoad(Box::pin(fut));
                // Immediately poll the awaiting load to start it working:
                trace!("Empty state -> AwaitingLoad. calling wake_by_ref()");
                cx.waker().wake_by_ref();
                return Poll::Pending;
            },
            AwaitingLoad(ref mut fut) => {
                match fut.poll_unpin(cx) {
                    Poll::Pending => {
                        return Poll::Pending;
                    },
                    Poll::Ready(Err(err)) => {
                        return Poll::Ready(Err(io::Error::other(err)));
                    },
                    Poll::Ready(Ok(None)) => {
                        let err = Error::MissingEvent(self.block_to_load()); // TODO [1]
                        return Poll::Ready(Err(io::Error::other(err)));
                    }
                    Poll::Ready(Ok(Some(event))) => {
                        let bytes = match super::get_bytes(&event) {
                            Ok(b) => b,
                            Err(e) => {
                                return Poll::Ready(Err(io::Error::other(e)));
                            }
                        };
                        trace!("AwaitingLoad -> HaveBytes");
                        self.state = HaveBytes(bytes);
                        cx.waker().wake_by_ref();
                        return Poll::Pending;        
                    }
                }
            }
            HaveBytes(bytes) => {
                // TODO: [2]
                let mut slice = &bytes[block_offset..];
                let mut finished_bytes = false;
                if slice.len() > buf.remaining() {
                    slice = &slice[0..buf.remaining()];
                } else {
                    finished_bytes = true;
                }
                buf.put_slice(slice);
                self.offset += slice.len() as u64;

                if finished_bytes {
                    self.state = Empty;
                }

                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl AsyncSeekStart for MultipartRange {
    fn start_seek(mut self: Pin<&mut Self>, position: u64) -> io::Result<()> {
        // TODO: optimization: Could keep the bytes if we seek within them.
        self.state = State::Empty;
        self.offset = position;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl RangeBody for MultipartRange {
    fn byte_size(&self) -> u64 {
        self.init.total_size
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("missing event ID {0}")]
    MissingEvent(EventId),
}