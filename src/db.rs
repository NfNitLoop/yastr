use indoc::indoc;
use nostr::JsonUtil;
use std::{path::Path, sync::Arc, time::Duration};

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
}
