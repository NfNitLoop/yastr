mod commands;
mod db;
mod result;
mod server;

fn main() -> result::Result {
    commands::parse_and_run()
}
