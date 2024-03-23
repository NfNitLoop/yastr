use axum::routing::get;
use indoc::indoc;
use std::{net::SocketAddr, path::PathBuf, str::FromStr as _};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

use clap::Parser;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode},
    ConnectOptions as _,
};
use tokio::runtime::Runtime;

use crate::result::Result;
use crate::server;

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
struct ServeCommand {
    /// Should we open the browser to the URL we're serving?
    #[arg(long)]
    open: bool,
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
    // TODO: add debug logging:
    // println!("{cmd:?}");

    match cmd {
        Command::Db {
            command: db_command,
        } => match db_command {
            DbCommand::Init { options } => db_init(options),
        },
        Command::Serve(serve_cmd) => serve(serve_cmd),
    }
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

        sqlx::query(indoc! {"
            CREATE TABLE event(
                json TEXT NOT NULL,
                id BLOB NOT NULL GENERATED ALWAYS AS (unhex(json ->> '$.id')),
                pubkey BLOB NOT NULL GENERATED ALWAYS AS (unhex(json ->> '$.pubkey')),
                created_at INTEGER NOT NULL GENERATED ALWAYS AS (json ->> '$.created_at'),
                created_at_utc TEXT NOT NULL GENERATED ALWAYS AS (datetime(created_at, 'unixepoch'))
            ) STRICT
        "})
        .execute(&mut conn)
        .await?;

        sqlx::query(indoc! {"
            CREATE UNIQUE INDEX event_id ON event(id);
            CREATE INDEX event_created_at ON event(created_at, id);
            CREATE INDEX event_pubkey ON event(pubkey, created_at);
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
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,serve=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let app = axum::Router::new().route("/", get(server::get_root));

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
