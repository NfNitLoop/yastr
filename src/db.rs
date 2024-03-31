use futures::StreamExt;
use indoc::indoc;
use nostr::{JsonUtil, PublicKey};
use std::{path::Path, str::FromStr, sync::Arc, time::Duration};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::debug;

use sqlx::{
    sqlite::{SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    ConnectOptions as _, FromRow, Pool, Sqlite,
};

/// The interface to our underlying database.
#[derive(Clone)]
pub(crate) struct DB {
    pool: Arc<Pool<Sqlite>>,
}

impl DB {
    pub async fn init(db_file: &str) -> Result<(), sqlx::Error> {
        let mut conn = SqliteConnectOptions::from_str(db_file)?
            .journal_mode(SqliteJournalMode::Wal)
            .create_if_missing(true)
            .connect()
            .await?;

        debug!("create table");
        sqlx::query(indoc! {"
            CREATE TABLE event(
                jsonb BLOB NOT NULL
                , id TEXT NOT NULL AS (jsonb ->> '$.id')
                , kind INT NOT NULL AS (jsonb ->> '$.kind')
                , pubkey TEXT NOT NULL AS (jsonb ->> '$.pubkey')
                , signature TEXT NOT NULL AS (jsonb ->> '$.sig')
                , content TEXT NOT NULL AS (jsonb ->> '$.content')
                , created_at INTEGER NOT NULL AS (jsonb ->> '$.created_at')
                , created_at_utc TEXT NOT NULL AS (datetime(created_at, 'unixepoch'))
                , json TEXT NOT NULL AS (json(jsonb))
            ) STRICT
        "})
        .execute(&mut conn)
        .await?;

        debug!("create indexes");
        sqlx::query(indoc! {"
            CREATE UNIQUE INDEX event_id ON event(unhex(id)); -- Using unhex to save on index size.
            CREATE INDEX event_kind ON event(kind, created_at);
            CREATE INDEX event_created_at ON event(created_at, unhex(id));
            CREATE INDEX event_pubkey ON event(unhex(pubkey), created_at DESC);
            CREATE INDEX event_pubkey_kind ON event(unhex(pubkey), kind, created_at DESC); -- Useful for finding the latest profile/follow-list, etc
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE VIEW tag_view AS
            SELECT
                e.id AS event_id
                , t.value ->> '$[0]' AS name
                , t.value ->> '$[1]' AS value
                , t.value ->> '$[2]' AS value2
                , t.value ->> '$[2]' AS value3
                , t.id AS json_id
            FROM
                event AS e
                , json_each(jsonb, '$.tags') AS t
            ORDER BY 
                unhex(event_id)
                , json_id
            ;
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE TABLE tag( -- 'materialized view' of event tags
                event_id BLOB NOT NULL
                , name TEXT NOT NULL
                , value TEXT NOT NULL
            );
            CREATE INDEX tag_event ON tag(event_id);
            CREATE INDEX tag_name_value ON tag(name, value);
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE TRIGGER event_tags AFTER INSERT ON event
            BEGIN
                INSERT INTO tag(event_id, name, value)
                SELECT unhex(t.event_id), name, value
                FROM tag_view AS t
                WHERE unhex(t.event_id) = unhex(new.id)
                AND length(t.name) = 1;
            END;
            
            CREATE TRIGGER event_tags_delete AFTER DELETE ON event
            BEGIN
                DELETE FROM tag WHERE event_id = unhex(old.id);
            END;
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE VIEW event_latest_kind AS
            WITH k AS (
                SELECT
                    e.pubkey
                    , e.kind
                    , e.id AS event_id
                    , row_number() OVER (
                        PARTITION BY unhex(e.pubkey), e.kind 
                        ORDER BY unhex(e.pubkey), e.kind, created_at DESC
                    ) AS row_number
                FROM event AS e
            )
            SELECT
                pubkey, kind, event_id
            FROM k
            WHERE row_number = 1;
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE VIEW profile_info AS
            SELECT
                elk.pubkey
                , e.content ->> '$.name' AS name
                , e.content ->> '$.display_name' AS display_name
                , e.content ->> '$.username' AS username
                , e.content ->> '$.nip05' AS nip05
                , e.content ->> '$.about' AS about
            FROM
                event_latest_kind AS elk
                JOIN event AS e ON (unhex(e.id) = unhex(elk.event_id))
            WHERE
                elk.kind = 0;
            SELECT * from profile_info;
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE VIEW follows AS
            SELECT
                elk.pubkey AS follower
                , t.value AS followed
            FROM
                event_latest_kind AS elk
                JOIN tag AS t ON (t.event_id = unhex(elk.event_id) AND t.name = 'p')
            WHERE elk.kind = 3;
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE VIEW follows_named AS
            SELECT
                follower
                , pi1.name AS follower_name
                , followed
                , pi2.name AS followed_name
            FROM
                follows AS f
                -- JOIN event AS e ON (unhex(e.id) = unhex(elk.event_id))
                LEFT JOIN profile_info AS pi1 ON (unhex(pi1.pubkey) = unhex(follower))
                LEFT JOIN profile_info AS pi2 ON (unhex(pi2.pubkey) = unhex(followed))
            ;
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE TABLE allow_users(
                pubkey TEXT NOT NULL
                , note TEXT DEFAULT NULL
                , allowed_levels INTEGER NOT NULL DEFAULT 1
            )
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE VIEW allowed_users AS
            WITH RECURSIVE
                au(pubkey, level) AS (
                    SELECT pubkey, allowed_levels FROM allow_users
                    UNION
                    SELECT followed, au.level - 1
                    FROM au
                    JOIN follows ON (unhex(au.pubkey) = unhex(follows.follower))
                    WHERE au.level > 0
                )
            select * from au;
        "})
        .execute(&mut conn)
        .await?;

        Ok(())
    }

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
            INSERT INTO event(jsonb)
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
        let mut query = sqlx::QueryBuilder::new("SELECT json FROM event WHERE true");

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
                    query.push_unseparated(" AND unhex(pubkey) IN (");
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
                    query.push_unseparated(" AND unhex(id) IN (");
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

    pub(crate) async fn user_allowed(&self, pubkey: &PublicKey) -> Result<bool, sqlx::Error> {
        let row: (bool,) = sqlx::query_as(indoc! {"
            SELECT EXISTS (
                SELECT pubkey from allowed_users where pubkey = ?
            )"})
        .bind(pubkey.to_string())
        .fetch_one(self.pool.as_ref())
        .await?;

        Ok(row.0)
    }

    /// True iff this event is referred to by one on the server:
    pub(crate) async fn referred_event(&self, id: &nostr::EventId) -> Result<bool, sqlx::Error> {
        let row: HasRefRow = sqlx::query_as(indoc! {"
            SELECT EXISTS (
                SELECT name from tag where name = 'e' AND value = ?
            ) AS has_ref
        "})
        .bind(id.to_string())
        .fetch_one(self.pool.as_ref())
        .await?;

        Ok(row.has_ref)
    }

    /// True iff this pubkey is referred to by someone on this server.
    /// Includes if "someone" is by an event that uses this pubkey.
    pub(crate) async fn referred_pubkey(
        &self,
        pubkey: &nostr::PublicKey,
    ) -> Result<bool, sqlx::Error> {
        let row: HasRefRow = sqlx::query_as(indoc! {"
            SELECT EXISTS (
                SELECT name from tag where name = 'p' AND value = ?
            ) OR EXISTS (
                SELECT 1 from event where unhex(pubkey) = ?
            ) AS has_ref
        "})
        .bind(pubkey.to_string())
        .bind(pubkey.to_bytes().to_vec())
        .fetch_one(self.pool.as_ref())
        .await?;

        Ok(row.has_ref)
    }
}

#[derive(sqlx::FromRow, Debug)]
struct JsonRow {
    json: String,
}

#[derive(FromRow)]
struct HasRefRow {
    has_ref: bool,
}
