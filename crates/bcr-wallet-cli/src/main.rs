use tracing::info;
use tracing_subscriber::{filter::LevelFilter, prelude::*};

#[tokio::main]
async fn main() {
    tracing_log::LogTracer::init().expect("LogTracer init");
    let level_filter = LevelFilter::DEBUG;
    let stdout_log = tracing_subscriber::fmt::layer().with_filter(level_filter);
    let subscriber = tracing_subscriber::registry().with(stdout_log);
    tracing::subscriber::set_global_default(subscriber)
        .expect("tracing::subscriber::set_global_default");

    info!("Hello CLI Wallet");

    bcr_wallet_core::initialize_api().await;
}
