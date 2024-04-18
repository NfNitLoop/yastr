use axum::routing::get;

use std::{net::SocketAddr, path::PathBuf};
use tracing::debug;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

use clap::Parser;
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
        DB::init(db_file).await?;
        println!("Done.");
        Ok(())
    })
}

fn serve(opts: ServeCommand) -> Result {
    Runtime::new().unwrap().block_on(async_serve(opts))
}

async fn async_serve(opts: ServeCommand) -> Result {
    // TODO: Move into the server module.
    let db_pool = DB::connect(&opts.global.db_file).await?;
    let app = axum::Router::new()
        .route("/", get(server::get_root))
        .route("/nip95/:event_id", get(server::nip95::info))
        .route(
            "/nip95/:event_id/file/:file_name",
            get(server::nip95::get_file),
        )
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
