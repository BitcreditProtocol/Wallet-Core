use std::str::FromStr;

use anyhow::Result;
use bcr_wallet_core::AppState;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tracing::info;
use tracing_subscriber::{
    filter::{FilterFn, LevelFilter},
    prelude::*,
};

mod command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletSettings {
    pub mint_url: cashu::MintUrl,
    pub mnemonic: String,
    pub log_level: String,
    pub db_path: String,
}

#[derive(Parser)]
#[command(name = "cli-wallet")]
#[command(about = "A simple command line wallet")]
struct Cli {
    #[arg(short, long, default_value = "default")]
    wallet: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(name = "info")]
    Info,
    #[command(name = "clear")]
    Clear { id: usize },
    #[command(name = "add_wallet")]
    AddWallet,
    #[command(name = "restore_wallet")]
    RestoreWallet,
    #[command(name = "receive")]
    Receive { id: usize, token: String },
    #[command(name = "redeem")]
    Redeem { id: usize },
    #[command(name = "request_payment")]
    RequestPayment { id: usize, amount: u64 },
    #[command(name = "send_payment")]
    SendPayment { id: usize, input: String },
    #[command(name = "recover")]
    Recover { id: usize },
    #[command(name = "reclaim_funds")]
    ReclaimFunds { id: usize },
    #[command(name = "migrate_rabid")]
    MigrateRabid,
    #[command(name = "run_jobs")]
    RunJobs,
    #[command(name = "gen_mnemonic")]
    GenMnemonic,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let settings_file =
        std::env::var("SETTINGS_FILE").unwrap_or_else(|_| format!("{}.toml", cli.wallet));
    let settings = config::Config::builder()
        .add_source(config::File::with_name(&settings_file))
        .add_source(config::Environment::with_prefix("WLLT"))
        .build()?;

    let settings: WalletSettings = settings.try_deserialize()?;

    tracing_log::LogTracer::init().expect("LogTracer init");
    let level_filter = LevelFilter::from_str(&settings.log_level)?;
    let stdout_log = tracing_subscriber::fmt::layer()
        .with_filter(level_filter)
        .with_filter(FilterFn::new(|md| {
            md.target().starts_with("bcr_wallet_cli")
                || md.target().starts_with("bcr_wallet_core")
                || md.target().starts_with("bcr_wallet_lib")
        }));
    let subscriber = tracing_subscriber::registry().with(stdout_log);
    tracing::subscriber::set_global_default(subscriber)
        .expect("tracing::subscriber::set_global_default");

    println!("{LOGO}");

    let app_state = AppState::initialize(&settings.db_path).await?;

    match cli.command {
        Commands::Info => {
            info!(
                "Info for {}: {}",
                cli.wallet,
                command::cmd_info(&app_state).await?
            );
        }
        Commands::Clear { id } => {
            info!(
                "Clearing Wallet DB for {}: {}",
                cli.wallet,
                command::cmd_clear_wallet(&app_state, &cli.wallet, id).await?
            );
        }
        Commands::Receive { id, token } => {
            info!(
                "Receiving for {}: {}",
                cli.wallet,
                command::cmd_receive(&app_state, &cli.wallet, &token, id).await?
            );
        }
        Commands::Redeem { id } => {
            info!(
                "Redeeming for {}: {}",
                cli.wallet,
                command::cmd_redeem(&app_state, &cli.wallet, id).await?
            );
        }
        Commands::AddWallet => {
            info!(
                "Adding wallet for {}: {}",
                cli.wallet,
                command::cmd_add_wallet(&app_state, &settings, &cli.wallet).await?
            );
        }
        Commands::RestoreWallet => {
            info!(
                "Restoring wallet for {}: {}",
                cli.wallet,
                command::cmd_restore_wallet(&app_state, &settings, &cli.wallet).await?
            );
        }
        Commands::RequestPayment { id, amount } => {
            info!(
                "Requesting Payment for {}: {}, Amount: {amount}",
                cli.wallet,
                command::cmd_request_payment(&app_state, &cli.wallet, amount, id).await?
            );
        }
        Commands::SendPayment { id, input } => {
            info!(
                "Sending Payment for {}: {}, Input: {input}",
                cli.wallet,
                command::cmd_send_payment(&app_state, &cli.wallet, &input, id).await?
            );
        }
        Commands::GenMnemonic => {
            info!("{}", app_state.generate_random_mnemonic(12));
        }
        Commands::Recover { .. } => {
            info!("Recover for {}: NOT IMPLEMENTED", cli.wallet,);
        }
        Commands::ReclaimFunds { .. } => {
            info!("Reclaim for {}: NOT IMPLEMENTED", cli.wallet,);
        }
        Commands::MigrateRabid => {
            info!("Migrate Rabid for {}", cli.wallet,);
            app_state.purse_migrate_rabid().await?
        }
        Commands::RunJobs => {
            info!("RunJobs for {}:", cli.wallet);
            command::cmd_run_jobs(&app_state).await?;
        }
    }

    Ok(())
}

const LOGO: &str = r#"
______ _ _                    _ _ _     _    _       _ _      _   
| ___ (_) |                  | (_) |   | |  | |     | | |    | |  
| |_/ /_| |_ ___ _ __ ___  __| |_| |_  | |  | | __ _| | | ___| |_ 
| ___ \ | __/ __| '__/ _ \/ _` | | __| | |/\| |/ _` | | |/ _ \ __|
| |_/ / | || (__| | |  __/ (_| | | |_  \  /\  / (_| | | |  __/ |_ 
\____/|_|\__\___|_|  \___|\__,_|_|\__|  \/  \/ \__,_|_|_|\___|\__|
"#;
