mod commands;
mod result;


fn main() -> result::Result {
    commands::parse_and_run()
}
