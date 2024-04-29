mod commands;
mod db;
mod result;
mod server;
mod sqlx_extras;

fn main() -> result::Result {
    commands::parse_and_run()
}
