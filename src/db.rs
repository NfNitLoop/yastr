use futures::StreamExt;
use indoc::indoc;
use nostr::JsonUtil;
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc::{Receiver, Sender};

use sqlx::{
    sqlite::{SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    Pool, Sqlite,
};

/// The interface to our underlying database.
#[derive(Clone)]
pub(crate) struct DB {
    pool: Arc<Pool<Sqlite>>,
}

impl DB {
    /// Create a DB pool, and also open an initial connection to test it.
    pub async fn connect(db_file: &Path) -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(20)
            .idle_timeout(Duration::from_secs(60))
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(
                SqliteConnectOptions::new()
                    .auto_vacuum(SqliteAutoVacuum::Incremental)
                    .journal_mode(SqliteJournalMode::Wal)
                    .filename(db_file),
            )
            .await?;

        Ok(Self {
            pool: Arc::new(pool),
        })
    }

    /// Save an event to the DB.
    ///
    /// Note: Assumes you've already validated the event.
    pub async fn save_event(&self, event: &nostr::Event) -> Result<(), sqlx::Error> {
        let query = sqlx::query(indoc! {"
            INSERT INTO event(json)
            VALUES (jsonb(?))
        "})
        .bind(event.as_json());
        query.execute(&*self.pool).await?;
        Ok(())
    }

    /// Search for events matching the given filters.
    /// Returns a receiver channel onto which the results will be delivered.
    pub(crate) fn search(
        &self,
        filters: Vec<nostr::Filter>,
    ) -> Receiver<Result<nostr::Event, sqlx::Error>> {
        let (sender, receiver) = tokio::sync::mpsc::channel(10);

        let db = self.clone();
        tokio::spawn(async move { db._search(filters, sender).await });
        receiver
    }

    async fn _search(
        &self,
        filters: Vec<nostr::Filter>,
        sender: Sender<Result<nostr::Event, sqlx::Error>>,
    ) {
        let mut query = sqlx::QueryBuilder::new("SELECT json(json) AS json FROM event WHERE true");

        if let Some(filter) = filters.first() {
            let nostr::Filter {
                authors,
                generic_tags: _, // TODO
                ids,
                kinds,
                limit,
                search: _,
                since,
                until,
            } = filter;
            if let Some(authors) = authors {
                if !authors.is_empty() {
                    let mut query = query.separated(",");
                    query.push_unseparated(" AND pubkey IN (");
                    for pubkey in authors {
                        let key = pubkey.to_bytes().to_vec();
                        query.push_bind(key);
                    }
                    query.push_unseparated(")");
                }
            }
            if let Some(until) = until {
                // Note: NIP-1 docs this as non-inclusive.
                // I've seen other nips use since <= event <= until, but that seems wrong.
                query.push(" AND created_at < ");
                query.push_bind(until.as_i64());
            }
            if let Some(since) = since {
                query.push(" AND created_at >= ");
                query.push_bind(since.as_i64());
            }

            if let Some(ids) = ids {
                if !ids.is_empty() {
                    let mut query = query.separated(",");
                    query.push_unseparated(" AND id IN (");
                    for id in ids {
                        query.push_bind(id.as_bytes().to_vec());
                    }
                    query.push_unseparated(")");
                }
            }

            if let Some(kinds) = kinds {
                if !kinds.is_empty() {
                    let mut query = query.separated(",");
                    query.push_unseparated(" AND kind IN (");
                    for kind in kinds {
                        query.push_bind(kind.as_u64() as i64);
                    }
                    query.push_unseparated(")");
                }
            }

            let limit = limit.unwrap_or(100) as i64;
            query.push(" LIMIT ");
            query.push_bind(limit);
        }

        let mut stream = query.build_query_as().fetch(self.pool.as_ref());
        while let Some(row) = stream.next().await {
            let row: JsonRow = match row {
                Err(err) => {
                    let _ = sender.send(Err(err)).await;
                    return;
                }
                Ok(row) => row,
            };
            let event = match nostr::Event::from_json(row.json) {
                Ok(event) => event,
                Err(err) => {
                    let res = Err(sqlx::Error::ColumnDecode {
                        index: "event.json".into(),
                        source: err.into(),
                    });
                    let _ = sender.send(res).await;
                    return;
                }
            };
            let res = sender.send(Ok(event)).await;
            if res.is_err() {
                return;
            }
        }
    }
}

#[derive(sqlx::FromRow, Debug)]
struct JsonRow {
    json: String,
}
