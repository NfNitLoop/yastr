use axum::routing::get;
use indoc::indoc;
use std::{net::SocketAddr, path::PathBuf, str::FromStr as _};
use tracing::debug;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

use clap::Parser;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
    ConnectOptions as _,
};
use tokio::runtime::Runtime;

use crate::server;
use crate::{db::DB, result::Result};

#[derive(Parser, Debug)]
#[command(version, about)]
pub enum Command {
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    Serve(ServeCommand),
}

#[derive(Parser, Debug)]
pub struct ServeCommand {
    // /// Should we open the browser to the URL we're serving?
    // #[arg(long)]
    // open: bool,
    #[command(flatten)]
    global: GlobalOptions,
}

#[derive(Parser, Debug)]
pub struct GlobalOptions {
    #[arg(long, default_value = "yastr.sqlite3")]
    db_file: PathBuf,
}

#[derive(Parser, Debug)]
pub enum DbCommand {
    Init {
        #[command(flatten)]
        options: GlobalOptions,
    },
}

pub fn parse_and_run() -> Result {
    let cmd = Command::parse();
    init_logging();
    debug!("Parsed command: {cmd:?}");

    match cmd {
        Command::Db {
            command: db_command,
        } => match db_command {
            DbCommand::Init { options } => db_init(options),
        },
        Command::Serve(serve_cmd) => serve(serve_cmd),
    }
}

fn init_logging() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

fn db_init(options: GlobalOptions) -> Result {
    if options.db_file.exists() {
        return Err("File already exists".to_string().into());
    }

    let db_file = options
        .db_file
        .to_str()
        .expect("file name should be representable in utf8");

    Runtime::new().unwrap().block_on(async move {
        println!("Initializing {}...", db_file);

        let mut conn = SqliteConnectOptions::from_str(db_file)?
            .journal_mode(SqliteJournalMode::Wal)
            .create_if_missing(true)
            .connect()
            .await?;

        // let exec = |sql: &'static str| async { sqlx::query(sql).execute(&mut conn) };

        debug!("create table");
        sqlx::query(indoc! {"
            CREATE TABLE event(
                json BLOB NOT NULL,
                id BLOB NOT NULL GENERATED ALWAYS AS (unhex(json ->> '$.id')),
                kind INT NOT NULL GENERATED ALWAYS AS (json ->> '$.kind'),
                pubkey BLOB NOT NULL GENERATED ALWAYS AS (unhex(json ->> '$.pubkey')),
                created_at INTEGER NOT NULL GENERATED ALWAYS AS (json ->> '$.created_at'),
                created_at_utc TEXT NOT NULL GENERATED ALWAYS AS (datetime(created_at, 'unixepoch'))
            ) STRICT
        "})
        .execute(&mut conn)
        .await?;

        debug!("create indexes");
        sqlx::query(indoc! {"
            CREATE UNIQUE INDEX event_id ON event(id);
            CREATE INDEX event_kind ON event(kind, created_at);
            CREATE INDEX event_created_at ON event(created_at, id);
            CREATE INDEX event_pubkey ON event(pubkey, created_at);
            -- Useful for finding the latest profile/follow-list, etc:
            CREATE INDEX event_pubkey_kind ON event(pubkey, kind, created_at);
            -- TODO: The same for the 'd' tag for a user.
        "})
        .execute(&mut conn)
        .await?;

        debug!("create view");
        sqlx::query(indoc! {"
            -- event_view: a more human-readable version of the events table. (except for raw_id)
            CREATE VIEW event_view AS
            SELECT
                id AS raw_id, -- to easily join w/ events table.
                (json ->> '$.id') AS id,
                kind,
                (json ->> '$.pubkey') AS pubkey,
                (json ->> '$.sig') AS signature,
                created_at,
                created_at_utc,
                (datetime(created_at, 'unixepoch', 'localtime')) AS created_at_local,
                json(json) AS json
            FROM event
            ORDER BY created_at DESC
        "})
        .execute(&mut conn)
        .await?;

        println!("Done.");

        Ok(())
    })
}

fn serve(opts: ServeCommand) -> Result {
    Runtime::new().unwrap().block_on(async_serve(opts))
}

async fn async_serve(opts: ServeCommand) -> Result {
    let db_pool = DB::connect(&opts.global.db_file).await?;
    let app = axum::Router::new()
        .route("/", get(server::get_root))
        .with_state(db_pool);

    // todo:
    let bind = "127.0.0.1:8095";
    println!("listening on: {bind}");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}
