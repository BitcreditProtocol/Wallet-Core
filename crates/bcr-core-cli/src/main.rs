// ----- standard library imports
use std::io::{self, Write};
use std::str::FromStr;
// ----- extra library imports
use bcr_wdc_webapi::keys::ActivateKeysetRequest;
use bcr_wdc_webapi::quotes::{EnquireReply, ResolveOffer};
use bcr_wdc_webapi::quotes::{
    EnquireRequest, SignedEnquireRequest, StatusReply, UpdateQuoteRequest, UpdateQuoteResponse,
};
use cashu::MintBolt11Request;

use cashu::nuts::nut02 as cdk02;
use tracing::info;
use tracing_subscriber::filter::LevelFilter;
// ----- local modules
mod clients;
mod test_utils;
mod wallet;
use clients::*;
use uuid::Uuid;
use wallet::Wallet;
// ----- end imports

#[derive(Debug, serde::Deserialize)]
struct MainConfig {
    user_service: String,
}

fn setup_tracing() {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::INFO)
        .init();
}

enum Action {
    RequestQuote(u64),
    MintEbill(Uuid, cashu::Id, u64),
    GetStatus(Uuid),
    ListKeysets,
    Resolve(Uuid, ResolveOffer),
    Exit,
    Invalid,
}

fn parse_action() -> Action {
    // Prompt (optional)
    print!("> ");
    io::stdout().flush().unwrap();

    // Read the whole line
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    // Split into words
    let mut parts = input.split_whitespace();
    // The command is the first word
    let cmd = parts.next().unwrap_or("");

    match cmd {
        "exit" => Action::Exit,
        "list_keysets" => Action::ListKeysets,

        "request_quote" => {
            // Expect exactly one more argument: the amount
            let amount_str = parts
                .next()
                .unwrap_or_else(|| panic!("request_quote requires an amount"));
            let amount = amount_str
                .parse::<u64>()
                .unwrap_or_else(|e| panic!("failed to parse amount `{}`: {}", amount_str, e));
            Action::RequestQuote(amount)
        }

        "mint_ebill" => {
            // Expect three more args: uuid, cashu_id, amount
            let uuid_str = parts
                .next()
                .unwrap_or_else(|| panic!("mint_ebill requires a UUID"));
            let id_str = parts
                .next()
                .unwrap_or_else(|| panic!("mint_ebill requires a cashu::Id"));
            let amount_str = parts
                .next()
                .unwrap_or_else(|| panic!("mint_ebill requires an amount"));

            // Parse UUID
            let uuid = Uuid::parse_str(uuid_str)
                .unwrap_or_else(|e| panic!("invalid UUID `{}`: {}", uuid_str, e));
            // Parse cashu::Id
            let cashu_id = cashu::Id::from_str(id_str)
                .unwrap_or_else(|e| panic!("invalid cashu::Id `{}`: {}", id_str, e));
            // Parse amount
            let amount = amount_str
                .parse::<u64>()
                .unwrap_or_else(|e| panic!("failed to parse amount `{}`: {}", amount_str, e));

            Action::MintEbill(uuid, cashu_id, amount)
        }

        "get_status" => {
            // Expect one more arg: uuid
            let uuid_str = parts
                .next()
                .unwrap_or_else(|| panic!("get_status requires a UUID"));
            let uuid = Uuid::parse_str(uuid_str)
                .unwrap_or_else(|e| panic!("invalid UUID `{}`: {}", uuid_str, e));
            Action::GetStatus(uuid)
        }

        "resolve" => {
            let qid = parts
                .next()
                .unwrap_or_else(|| panic!("resolve requires a quote ID"));
            let decision = parts
                .next()
                .unwrap_or_else(|| panic!("resolve requires a decision"));
            let resolution = if decision == "accept" {
                ResolveOffer::Accept
            } else if decision == "reject" {
                ResolveOffer::Reject
            } else {
                panic!("Invalid decision: `{}`", decision);
            };
            let uuid = Uuid::parse_str(qid).unwrap();
            Action::Resolve(uuid, resolution)
        }

        other => Action::Invalid,
    }
}

fn print_status() {
    println!("> request_quote <amount>");
    println!("> mint_ebill <uuid> <keyset_id> <amount>");
    println!("> get_status <uuid>");
    println!("> list_keysets");
    println!("> resolve <uuid> <accept/reject>");
    println!("> exit");
}

async fn run_wallet(cfg: &MainConfig) {
    setup_tracing();

    info!("WDC Client");
    print_status();

    let user_service = Service::<UserService>::new(cfg.user_service.clone());

    // Create wallet and Ebill
    let mut wallet = Wallet::new();

    loop {
        // Parse Action from Command Line Stdin
        let action = parse_action();

        match action {
            Action::ListKeysets => {
                let keysets = user_service.list_keysets().await;
                info!(keysets=?keysets, "Keysets");
            }
            Action::RequestQuote(amount) => {
                let (signed_request, signature) =
                    wallet.create_random_ebill(wallet.public_key().into(), amount);

                let bill_amount = signed_request.request.content.sum;

                info!(
                    bill_amount = bill_amount,
                    bill_id = signed_request.request.content.id,
                    "Bill created"
                );

                // Mint Ebill
                info!("Requesting to mint the bill");
                let enquire_reply: EnquireReply =
                    user_service.mint_credit_quote(signed_request).await;
                let quote_id = enquire_reply.id;

                info!(quote_id = ?quote_id, "Mint Request Accepted, waiting for admin to process");
            }
            Action::GetStatus(quote_id) => {
                let mint_quote_status_reply = user_service.lookup_credit_quote(quote_id).await;
                info!(quote_id=?quote_id, "Getting mint quote status for quote");

                let status = match mint_quote_status_reply {
                    StatusReply::Denied { .. } => "Denied",
                    StatusReply::Pending => "Pending",
                    StatusReply::Offered { .. } => "Offered",
                    StatusReply::Accepted { .. } => "Accepted",
                    StatusReply::Rejected { .. } => "Rejected",
                    StatusReply::Canceled { .. } => "Cancelled",
                    StatusReply::OfferExpired { .. } => "Expired",
                };
                if let StatusReply::Offered {
                    keyset_id,
                    expiration_date,
                    discounted,
                } = mint_quote_status_reply
                {
                    info!(keyset_id=%keyset_id, expiration_date=?expiration_date, discounted=?discounted, "Quote is offered");
                } else {
                    info!("Quote is not accepted - {}", status);
                }
            }
            Action::MintEbill(quote_id, keyset_id, amount) => {
                let (req, rs, secrets) = wallet.create_mint_request(quote_id, keyset_id, amount);

                info!("Sending NUT20 mint request");
                let mint_response = user_service.mint_ebill(req).await;
                let total = mint_response
                    .signatures
                    .iter()
                    .map(|s| u64::from(s.amount))
                    .sum::<u64>();
                info!(amount = total, "Mint Successful obtained signatures");

                let keys = user_service.list_keys(keyset_id).await;
                let keys = keys.keysets.first().unwrap();

                let proofs = cashu::dhke::construct_proofs(
                    mint_response.signatures,
                    rs,
                    secrets,
                    &keys.keys,
                )
                .unwrap();

                for p in &proofs {
                    info!(c=?p.c, amount=?p.amount, "Importing Proof");
                }

                let mint_url = cashu::MintUrl::from_str("http://example.com".into()).unwrap();
                let token = cashu::nut00::Token::new(
                    mint_url,
                    proofs,
                    None,
                    cashu::CurrencyUnit::Custom("crsat".into()),
                );
                info!(v3 = token.to_v3_string(), "Token")
            }
            Action::Resolve(uuid, resolution) => {
                info!("Resolving quote");
                let resolution = user_service.resolve_quote(uuid, resolution).await;
                info!(resolution=?resolution, "Quote resolved");
            }
            Action::Exit => {
                info!("Exiting...");
                break;
            }
            Action::Invalid => print_status(),
        }
    }
}

#[tokio::main]
async fn main() {
    let settings = config::Config::builder()
        .add_source(config::File::with_name("config.toml"))
        .add_source(config::Environment::with_prefix("UserWallet"))
        .build()
        .expect("Failed to build wildcat config");

    let cfg: MainConfig = settings
        .try_deserialize()
        .expect("Failed to parse configuration");

    run_wallet(&cfg).await;
}
