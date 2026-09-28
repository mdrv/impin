mod cli;
mod content;
mod daemon;
mod notice;
mod pin;
mod platform;
mod state;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();
    if let Err(err) = cli::run() {
        log::error!("{err:#}");
        std::process::exit(1);
    }
}
