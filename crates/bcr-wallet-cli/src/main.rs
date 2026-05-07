use std::{collections::HashMap, path::PathBuf, str::FromStr};

use anyhow::Result;
use bcr_wallet_api::{
    AppState, config::AppStateConfig, generate_random_mnemonic, get_wallet_id, is_valid_token,
};
use clap::{Parser, Subcommand};
use nostr_sdk::RelayUrl;
use serde::{Deserialize, Serialize};
use tracing::info;
use tracing_subscriber::{
    filter::{FilterFn, LevelFilter},
    prelude::*,
};

mod command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliSettings {
    pub log_level: String,
    pub db_path: PathBuf,
    pub wallets: HashMap<String, WalletSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletSettings {
    pub mint_url: bcr_common::cashu::MintUrl,
    pub mnemonic: bip39::Mnemonic,
    pub network: bitcoin::Network,
    pub nostr_relays: Vec<RelayUrl>,
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
    #[command(name = "add_wallet")]
    AddWallet { id: String },
    #[command(name = "delete_wallet")]
    DeleteWallet { id: String },
    #[command(name = "restore_wallet")]
    RestoreWallet { id: String },
    #[command(name = "receive")]
    Receive { id: String, token: String },
    #[command(name = "request_payment")]
    RequestPayment {
        id: String,
        amount: u64,
        description: Option<String>,
    },
    #[command(name = "send_payment")]
    SendPayment { id: String, input: String },
    #[command(name = "pay_by_token")]
    PayByToken {
        id: String,
        amount: u64,
        description: Option<String>,
    },
    #[command(name = "reclaim")]
    Reclaim { id: String, tx_id: String },
    #[command(name = "recover_stale")]
    RecoverStale { id: String },
    #[command(name = "melt")]
    Melt {
        id: String,
        amount: u64,
        address: String,
        description: Option<String>,
    },
    #[command(name = "mint")]
    Mint { id: String, amount: u64 },
    #[command(name = "protest_mint")]
    ProtestMint { id: String, quote_id: String },
    #[command(name = "protest_swap")]
    ProtestSwap { id: String, commitment_sig: String },
    #[command(name = "protest_melt")]
    ProtestMelt { id: String, quote_id: String },
    #[command(name = "migrate_rabid")]
    MigrateRabid,
    #[command(name = "run_jobs")]
    RunJobs,
    #[command(name = "gen_mnemonic")]
    GenMnemonic { network: bitcoin::Network },
    #[command(name = "wallet_id")]
    WalletId {
        network: bitcoin::Network,
        #[arg(num_args = 12..=24)]
        mnemonic: Vec<String>,
    },
    #[command(name = "check_token")]
    CheckToken { token: String },
    #[command(name = "check_rabid_offline")]
    CheckRabidOffline { id: String },
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

    let settings: CliSettings = settings.try_deserialize()?;

    tracing_log::LogTracer::init().expect("LogTracer init");
    let level_filter = LevelFilter::from_str(&settings.log_level)?;
    let stdout_log = tracing_subscriber::fmt::layer()
        .with_filter(level_filter)
        .with_filter(FilterFn::new(|md| {
            md.target().starts_with("bcr_wallet_cli")
                || md.target().starts_with("bcr_wallet_core")
                || md.target().starts_with("bcr_wallet_persistence")
                || md.target().starts_with("bcr_wallet_api")
        }));
    let subscriber = tracing_subscriber::registry().with(stdout_log);
    tracing::subscriber::set_global_default(subscriber)
        .expect("tracing::subscriber::set_global_default");

    println!("{LOGO}");

    let app_state_cfg = AppStateConfig {
        db_path: settings.db_path.clone(),
        mnemonics: settings
            .wallets
            .iter()
            .map(|(wid, w)| (wid.to_owned(), w.mnemonic.to_owned()))
            .collect(),
        swap_expiry: chrono::TimeDelta::minutes(15),
        dev_mode: true,
    };
    let app_state = AppState::initialize(app_state_cfg).await?;

    match cli.command {
        Commands::Info => {
            info!(
                "Info for {}: {}",
                cli.wallet,
                command::cmd_info(&app_state).await?
            );
        }
        Commands::Receive { id, token } => {
            info!(
                "Receiving for {}: {}",
                cli.wallet,
                command::cmd_receive(&app_state, &cli.wallet, &token, &id).await?
            );
        }
        Commands::AddWallet { id } => {
            info!(
                "Adding wallet for {}: {}",
                cli.wallet,
                command::cmd_add_wallet(&app_state, &cli.wallet, &settings.wallets[&id]).await?
            );
        }
        Commands::DeleteWallet { id } => {
            info!(
                "Deleting wallet for {}: {}",
                cli.wallet,
                command::cmd_delete_wallet(&app_state, &cli.wallet, &id).await?
            );
        }
        Commands::RestoreWallet { id } => {
            info!(
                "Restoring wallet for {}: {}",
                cli.wallet,
                command::cmd_restore_wallet(&app_state, &cli.wallet, &settings.wallets[&id])
                    .await?
            );
        }
        Commands::RequestPayment {
            id,
            amount,
            description,
        } => {
            info!(
                "Requesting Payment for {}: {}, Amount: {amount}, Description: {description:?}",
                cli.wallet,
                command::cmd_request_payment(
                    &app_state,
                    &cli.wallet,
                    amount,
                    &id,
                    description.clone()
                )
                .await?
            );
        }
        Commands::SendPayment { id, input } => {
            info!(
                "Sending Payment for {}: {}, Input: {input}",
                cli.wallet,
                command::cmd_send_payment(&app_state, &cli.wallet, &input, &id).await?
            );
        }
        Commands::PayByToken {
            id,
            amount,
            description,
        } => {
            info!(
                "Payment by Token for {}: {}, Amount: {amount}, Description: {description:?}",
                cli.wallet,
                command::cmd_pay_by_token(
                    &app_state,
                    &cli.wallet,
                    &id,
                    amount,
                    description.clone()
                )
                .await?
            );
        }
        Commands::GenMnemonic { network } => {
            let (mnemonic, wallet_id) = generate_random_mnemonic(12, network);
            info!("Wallet ID: {}", wallet_id);
            info!("Mnemonic: {}", mnemonic);
        }
        Commands::WalletId { network, mnemonic } => {
            let mnemonic = mnemonic.join(" ");
            let mnemonic = bip39::Mnemonic::from_str(&mnemonic).expect("is a valid mnemonic");
            let wallet_id = get_wallet_id(&mnemonic, network);
            info!("Wallet ID: {}", wallet_id);
        }
        Commands::Reclaim { id, tx_id } => {
            info!(
                "Reclaim for {}: {}",
                cli.wallet,
                command::cmd_reclaim(&app_state, &cli.wallet, &id, &tx_id).await?
            );
        }
        Commands::RecoverStale { id } => {
            info!(
                "Recover Stale proofs for {}: {}",
                cli.wallet,
                command::cmd_recover_stale(&app_state, &cli.wallet, &id).await?
            );
        }
        Commands::Melt {
            id,
            amount,
            address,
            description,
        } => {
            info!(
                "Melt for {}: {}",
                cli.wallet,
                command::cmd_melt(&app_state, &cli.wallet, &id, amount, &address, &description)
                    .await?
            );
        }
        Commands::Mint { id, amount } => {
            info!(
                "Mint for {}: {}",
                cli.wallet,
                command::cmd_mint(&app_state, &cli.wallet, &id, amount).await?
            );
        }
        Commands::ProtestMint { id, quote_id } => {
            info!(
                "Protest Mint for {}: {}",
                cli.wallet,
                command::cmd_protest_mint(&app_state, &cli.wallet, &id, &quote_id).await?
            );
        }
        Commands::ProtestSwap { id, commitment_sig } => {
            info!(
                "Protest Swap for {}: {}",
                cli.wallet,
                command::cmd_protest_swap(&app_state, &cli.wallet, &id, &commitment_sig).await?
            );
        }
        Commands::ProtestMelt { id, quote_id } => {
            info!(
                "Protest Melt for {}: {}",
                cli.wallet,
                command::cmd_protest_melt(&app_state, &cli.wallet, &id, &quote_id).await?
            );
        }
        Commands::MigrateRabid => {
            info!(
                "Migrate Rabid for {}: {}",
                cli.wallet,
                command::cmd_migrate_rabid(&app_state, &cli.wallet).await?
            )
        }
        Commands::RunJobs => {
            info!("RunJobs for {}:", cli.wallet);
            command::cmd_run_jobs(&app_state).await?;
        }
        Commands::CheckToken { token } => {
            info!("Checking token for {}:", cli.wallet);
            info!("{}", is_valid_token(&token)?);
        }
        Commands::CheckRabidOffline { id } => {
            info!(
                "Check Rabid/Offline for {} and wallet {}: Rabid: {}, Offline: {}",
                cli.wallet,
                id,
                app_state.wallet_mint_is_rabid(id.clone()).await?,
                app_state.wallet_mint_is_offline(id.clone()).await?,
            );
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
