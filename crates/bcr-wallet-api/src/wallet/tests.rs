use ::nostr::{
    event::EventId,
    nips::nip19::{FromBech32, Nip19Profile},
    types::RelayUrl,
};
use bcr_common::{
    cashu::ProofsMethods as CashuProofsMethods,
    core_tests,
    ecash::{self, ProofsMethods},
    wire::{
        clowder as wire_clowder,
        common::{self as wire_common, ProtestOutcome},
    },
};
use bcr_wallet_core::{
    contact::Contact,
    event::{ContactPaymentRequestPayload, PaymentRequestActionKind, PaymentRequestActionPayload},
    name::Name,
    types::{
        ClowderBeta, ForeignMintProofReason, PaymentRequestDirection, PaymentRequestState,
        PaymentRequestTransitionOutcome, PaymentResultCallback, TimeRange, TransactionFees,
    },
};
use bcr_wallet_persistence::PocketRepository;
use bcr_wallet_persistence::{
    MockContactStoreApi, MockMigrationJournalRepository, MockNostrRepository,
    MockPaymentRequestStoreApi, MockTransactionRepository,
    test_utils::tests::{test_other_pub_key, test_pub_key, zero_seed},
};
use bcr_wallet_transport::{NostrEventChannel, error::Error as TransportError};
use bitcoin::base64::{Engine, engine::general_purpose::STANDARD};
use secp256k1::SECP256K1;
use std::str::FromStr;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::*;
use crate::{
    external::mint::{HttpClientExt, MockClowderMintConnector},
    pocket::{
        PocketBalance,
        debit::{MeltProtestResult, ProtestResult},
        test_utils::tests::{
            MockDebitPocket, mock_commitment_result, setup_attestation_mock, test_kinfos,
        },
    },
    wallet::{
        api::WalletApi,
        test_utils::tests::{MockConsumer, MockTransport},
        types::WalletPaymentType,
    },
};

const NODE_ID_1: &str = "bitcrt03205b8dec12bc9e879f5b517aa32192a2550e88adcee3e54ec2c7294802568fef";
const NODE_ID_2: &str = "bitcrt0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

fn node_id(value: &str) -> NodeId {
    value.parse().expect("valid node id")
}

fn name(value: &str) -> Name {
    Name::from_str(value).expect("valid name")
}

fn test_contact() -> Contact {
    Contact {
        id: Uuid::new_v4(),
        node_id: Some(node_id(NODE_ID_1)),
        email: None,
        name: Some(name("Minka")),
        company: None,
        nostr_relays: vec![],
    }
}

fn payment_request_with(
    id: Uuid,
    direction: PaymentRequestDirection,
    state: PaymentRequestState,
) -> PaymentRequest {
    PaymentRequest {
        id,
        node_id: Some(node_id(NODE_ID_1)),
        amount: Amount::from(42),
        unit: CurrencyUnit::Sat,
        description: Some("payment request memo".to_string()),
        deadline: Some(999),
        created_at: 123,
        state,
        direction,
        history: Vec::new(),
        tombstone: false,
    }
}

fn test_keyset_and_proofs(
    amounts: &[Amount],
) -> (
    ecash::KeySetInfo,
    bcr_common::ecash::MintKeySet,
    Vec<cashu::Proof>,
) {
    let (info, keyset) = core_tests::generate_random_ecash_keyset();
    let proofs = core_tests::generate_random_ecash_proofs(&keyset, amounts);
    (ecash::KeySetInfo::from(info), keyset, proofs)
}

struct MockWalletCtx {
    pub client: MockClowderMintConnector,
    pub tx_repo: MockTransactionRepository,
    pub debit: MockDebitPocket,
    pub journal: MockMigrationJournalRepository,
    pub nostr_repo: MockNostrRepository,
    pub contact_repo: MockContactStoreApi,
    pub payment_request_repo: MockPaymentRequestStoreApi,
    pub nostr_transport: MockTransport,
    pub nostr_event_channel: NostrEventChannel,
    pub nostr_consumer: MockConsumer,
}

fn wallet_ctx() -> MockWalletCtx {
    let client = MockClowderMintConnector::new();
    MockWalletCtx {
        client,
        tx_repo: MockTransactionRepository::new(),
        debit: MockDebitPocket::new(),
        journal: MockMigrationJournalRepository::new(),
        nostr_repo: MockNostrRepository::new(),
        contact_repo: MockContactStoreApi::new(),
        payment_request_repo: MockPaymentRequestStoreApi::new(),
        nostr_transport: MockTransport::new(),
        nostr_event_channel: NostrEventChannel::new(),
        nostr_consumer: MockConsumer::new(),
    }
}

async fn wallet(mut ctx: MockWalletCtx) -> Arc<RwLock<Wallet>> {
    let debit = std::mem::replace(&mut ctx.debit, MockDebitPocket::new());
    wallet_with_debit(ctx, Box::new(debit)).await
}

async fn wallet_with_debit(
    ctx: MockWalletCtx,
    debit: Box<dyn DebitPocketApi>,
) -> Arc<RwLock<Wallet>> {
    let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(ctx.client);
    let mut beta_mock = crate::external::mint::MockClowderMintConnector::new();
    beta_mock.expect_get_alpha_status().returning(|_| {
        Ok(bcr_common::wire::clowder::AlphaStateResponse {
            state: bcr_common::wire::clowder::SimpleAlphaState::Online(0),
        })
    });
    beta_mock.expect_get_alpha_substitute().returning(|_| {
        Err(bcr_common::client::mint::Error::Internal(
            "no substitute".to_string(),
        ))
    });
    let beta_url = url::Url::parse("https://beta.test").unwrap();
    let mut beta_clients: BTreeMap<url::Url, Arc<dyn ClowderMintConnector>> = BTreeMap::new();
    beta_clients.insert(beta_url, Arc::new(beta_mock));

    Wallet::new(
        bitcoin::Network::Testnet,
        arc_client,
        HashMap::new(),
        Box::new(ctx.tx_repo),
        Arc::new(ctx.contact_repo),
        Box::new(ctx.payment_request_repo),
        debit,
        Arc::new(ctx.journal),
        "wallet-1".to_owned(),
        "w-1".to_owned(),
        test_pub_key(),
        test_pub_key(),
        beta_clients,
        Box::new(|url| Arc::new(HttpClientExt::new(url))),
        time::Duration::seconds(60),
        Arc::new(ctx.nostr_transport),
        ctx.nostr_event_channel,
        Arc::new(ctx.nostr_repo),
        Arc::new(ctx.nostr_consumer),
    )
    .await
}

async fn wallet_with_betas(
    w: Arc<RwLock<Wallet>>,
    betas: Vec<(url::Url, Arc<dyn ClowderMintConnector>)>,
) -> Arc<RwLock<Wallet>> {
    let mut map = BTreeMap::new();
    for (url, cl) in betas {
        map.insert(url, cl);
    }
    w.write().await.beta_clients = map;
    w
}

fn reclaimable_tx(amount: Amount) -> Transaction {
    Transaction {
        id: Uuid::new_v4(),
        mint_url: cashu::MintUrl::from_str("https://mint.example").unwrap(),
        direction: TransactionDirection::Outgoing,
        fees: TransactionFees::default(),
        amount,
        memo: None,
        payment_type: PaymentType::Token,
        status: TransactionStatus::Pending,
        tstamp: 123,
        unit: CurrencyUnit::Sat,
        ys: vec![],
        quote_id: None,
        payment_request_id: None,
        btc_tx_id: None,
        nostr_event_id: None,
        contact_node_id: None,
        linked_txs: vec![],
    }
}

fn tx_with(
    n: u64,
    timestamp: u64,
    amount: u64,
    direction: TransactionDirection,
    ptype: PaymentType,
    status: TransactionStatus,
) -> Transaction {
    Transaction {
        id: Uuid::new_v4(),
        mint_url: cashu::MintUrl::from_str("https://mint.example").unwrap(),
        direction,
        fees: TransactionFees::default(),
        amount: Amount::from(amount),
        memo: None,
        status,
        payment_type: ptype,
        tstamp: timestamp,
        unit: CurrencyUnit::Sat,
        ys: vec![test_cashu_pubkey(n)],
        quote_id: None,
        payment_request_id: None,
        btc_tx_id: None,
        nostr_event_id: None,
        contact_node_id: None,
        linked_txs: vec![],
    }
}

fn test_cashu_pubkey(n: u64) -> cashu::PublicKey {
    let mut sk_bytes = [1u8; 32];
    sk_bytes[24..].copy_from_slice(&(n + 1).to_be_bytes());
    let sk = secp256k1::SecretKey::from_slice(&sk_bytes).unwrap();
    let pk = secp256k1::PublicKey::from_secret_key(SECP256K1, &sk);
    cashu::PublicKey::from_slice(&pk.serialize()).unwrap()
}

fn sample_txs() -> Vec<Transaction> {
    vec![
        tx_with(
            1,
            100,
            50,
            TransactionDirection::Incoming,
            PaymentType::Token,
            TransactionStatus::Settled,
        ),
        tx_with(
            2,
            100,
            20,
            TransactionDirection::Outgoing,
            PaymentType::Token,
            TransactionStatus::Pending,
        ),
        tx_with(
            3,
            200,
            20,
            TransactionDirection::Incoming,
            PaymentType::PaymentRequest,
            TransactionStatus::Settled,
        ),
        tx_with(
            4,
            300,
            80,
            TransactionDirection::Outgoing,
            PaymentType::PaymentRequest,
            TransactionStatus::Canceled,
        ),
        tx_with(
            5,
            300,
            10,
            TransactionDirection::Incoming,
            PaymentType::Token,
            TransactionStatus::Pending,
        ),
        tx_with(
            6,
            400,
            80,
            TransactionDirection::Outgoing,
            PaymentType::Token,
            TransactionStatus::Settled,
        ),
    ]
}

fn sort_expected(mut txs: Vec<Transaction>, sort: TransactionSort) -> Vec<Transaction> {
    match sort {
        TransactionSort::TimeAsc => {
            txs.sort_by(|a, b| a.tstamp.cmp(&b.tstamp).then_with(|| a.id.cmp(&b.id)));
        }
        TransactionSort::TimeDesc => {
            txs.sort_by(|a, b| b.tstamp.cmp(&a.tstamp).then_with(|| b.id.cmp(&a.id)));
        }
        TransactionSort::AmountAsc => {
            txs.sort_by(|a, b| a.amount.cmp(&b.amount).then_with(|| a.id.cmp(&b.id)));
        }
        TransactionSort::AmountDesc => {
            txs.sort_by(|a, b| b.amount.cmp(&a.amount).then_with(|| b.id.cmp(&a.id)));
        }
    }

    txs
}

fn tx_ids(txs: &[Transaction]) -> Vec<Uuid> {
    txs.iter().map(|tx| tx.id).collect()
}

#[tokio::test]
async fn test_config_builds_expected_config() {
    let mut ctx = wallet_ctx();

    ctx.nostr_transport
        .expect_relays()
        .return_const(vec![RelayUrl::from_str("wss://test.example.com").unwrap()]);

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    let wlt = wallet(ctx).await;
    let cfg = wlt.read().await.config().expect("config works");

    assert_eq!(cfg.wallet_id, "w-1");
    assert_eq!(cfg.name, "wallet-1");
    assert_eq!(cfg.network, bitcoin::Network::Testnet);
    assert_eq!(cfg.debit, CurrencyUnit::Sat);
    assert_eq!(cfg.mint.to_string(), "https://mint.example/");
    assert_eq!(cfg.pub_key, test_pub_key());
    assert_eq!(cfg.clowder_id, test_pub_key());
    assert_eq!(cfg.betas.len(), 1);
}

#[tokio::test]
async fn test_check_received_payment_request_unknown_reports_none() {
    let mut ctx = wallet_ctx();
    let pid = Uuid::new_v4();
    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(|_| Ok(None));
    let wlt = wallet(ctx).await;

    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    let results_cb = results.clone();
    let callback: PaymentResultCallback = Arc::new(move |r| results_cb.lock().unwrap().push(r));

    wlt.read()
        .await
        .check_received_payment(
            std::time::Duration::from_secs(1),
            pid,
            CancellationToken::new(),
            callback,
        )
        .await
        .unwrap();

    assert_eq!(*results.lock().unwrap(), vec![None]);
}

#[tokio::test]
async fn test_check_received_payment_wakes_when_request_is_marked_paid() {
    let mut ctx = wallet_ctx();
    let pid = Uuid::new_v4();
    let tx_id = Uuid::new_v4();
    let (pending_read_tx, pending_read_rx) = tokio::sync::oneshot::channel();
    let mut seq = mockall::Sequence::new();
    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .in_sequence(&mut seq)
        .return_once(move |id| {
            pending_read_tx.send(()).expect("test receiver still alive");
            Ok(Some(payment_request_with(
                id,
                PaymentRequestDirection::Outgoing,
                PaymentRequestState::Pending,
            )))
        });
    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |id| {
            Ok(Some(payment_request_with(
                id,
                PaymentRequestDirection::Outgoing,
                PaymentRequestState::Paid { tx_id },
            )))
        });
    ctx.debit.expect_receive_proofs().never();
    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(|_, _| Ok(PaymentRequestTransitionOutcome::Applied));
    let wlt = wallet(ctx).await;

    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    let results_cb = results.clone();
    let callback: PaymentResultCallback = Arc::new(move |r| results_cb.lock().unwrap().push(r));

    let waiter = wlt.clone();
    let check = tokio::spawn(async move {
        waiter
            .read()
            .await
            .check_received_payment(
                std::time::Duration::from_secs(5),
                pid,
                CancellationToken::new(),
                callback,
            )
            .await
    });
    pending_read_rx.await.unwrap();
    wlt.read()
        .await
        .mark_payment_request_as_paid(
            pid,
            tx_id,
            node_id(NODE_ID_1),
            PaymentRequestActionOrigin::Local,
        )
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_millis(100), check)
        .await
        .expect("waiter should wake on the paid transition, not on a timer")
        .unwrap()
        .unwrap();
    assert_eq!(*results.lock().unwrap(), vec![Some(tx_id)]);
}

#[tokio::test]
async fn test_list_txs() {
    let mut ctx = wallet_ctx();
    ctx.tx_repo
        .expect_list_txs()
        .times(1)
        .returning(|| Ok(vec![]));
    let wlt = wallet(ctx).await;

    let res = wlt
        .read()
        .await
        .list_txs(
            TransactionFilters::default(),
            TransactionSort::default(),
            5,
            None,
        )
        .await
        .unwrap();
    assert!(res.txs.is_empty());
    assert!(res.next_cursor.is_none());
}

#[tokio::test]
async fn test_is_wallet_mint_offline_majority_true() {
    let ctx = wallet_ctx();
    let mut wlt = wallet(ctx).await;

    let b1 = url::Url::from_str("https://b1.example").unwrap();
    let b2 = url::Url::from_str("https://b2.example").unwrap();
    let b3 = url::Url::from_str("https://b3.example").unwrap();

    let mut m1 = MockClowderMintConnector::new();
    let mut m2 = MockClowderMintConnector::new();
    let mut m3 = MockClowderMintConnector::new();

    m1.expect_get_alpha_status().returning(|_pk| {
        Ok(wire_clowder::AlphaStateResponse {
            state: wire_clowder::SimpleAlphaState::Offline(0),
        })
    });
    m2.expect_get_alpha_status().returning(|_pk| {
        Ok(wire_clowder::AlphaStateResponse {
            state: wire_clowder::SimpleAlphaState::Offline(0),
        })
    });
    m3.expect_get_alpha_status().returning(|_pk| {
        Ok(wire_clowder::AlphaStateResponse {
            state: wire_clowder::SimpleAlphaState::Online(0),
        })
    });

    wlt = wallet_with_betas(
        wlt,
        vec![(b1, Arc::new(m1)), (b2, Arc::new(m2)), (b3, Arc::new(m3))],
    )
    .await;

    let res = wlt.read().await.is_wallet_mint_offline().await.unwrap();
    assert!(res);
}

#[tokio::test]
async fn test_is_wallet_mint_rabid_majority_false() {
    let ctx = wallet_ctx();
    let mut wlt = wallet(ctx).await;

    let b1 = url::Url::from_str("https://b1.example").unwrap();
    let b2 = url::Url::from_str("https://b2.example").unwrap();

    let mut m1 = MockClowderMintConnector::new();
    let mut m2 = MockClowderMintConnector::new();

    m1.expect_get_alpha_status().returning(|_pk| {
        Ok(wire_clowder::AlphaStateResponse {
            state: wire_clowder::SimpleAlphaState::Rabid("rabid".to_string()),
        })
    });
    m2.expect_get_alpha_status().returning(|_pk| {
        Ok(wire_clowder::AlphaStateResponse {
            state: wire_clowder::SimpleAlphaState::Online(0),
        })
    });

    wlt = wallet_with_betas(wlt, vec![(b1, Arc::new(m1)), (b2, Arc::new(m2))]).await;

    let res = wlt.read().await.is_wallet_mint_rabid().await.unwrap();
    assert!(!res);
}

#[tokio::test]
async fn test_mint_substitute_returns_some_on_majority_vote() {
    let ctx = wallet_ctx();
    let mut wlt = wallet(ctx).await;

    let b1 = url::Url::from_str("https://b1.example").unwrap();
    let b2 = url::Url::from_str("https://b2.example").unwrap();
    let b3 = url::Url::from_str("https://b3.example").unwrap();

    let substitute = url::Url::from_str("https://sub.example").unwrap();
    let other = url::Url::from_str("https://other.example").unwrap();

    let mut m1 = MockClowderMintConnector::new();
    let mut m2 = MockClowderMintConnector::new();
    let mut m3 = MockClowderMintConnector::new();

    m1.expect_get_alpha_substitute().returning({
        let substitute = substitute.clone();
        move |_pk| {
            Ok(wire_clowder::ConnectedMintResponse {
                mint: substitute.clone(),
                clowder: url::Url::from_str("https://clowder.example").unwrap(),
                node_id: test_pub_key(),
            })
        }
    });
    m2.expect_get_alpha_substitute().returning({
        let substitute = substitute.clone();
        move |_pk| {
            Ok(wire_clowder::ConnectedMintResponse {
                mint: substitute.clone(),
                clowder: url::Url::from_str("https://clowder.example").unwrap(),
                node_id: test_pub_key(),
            })
        }
    });
    m3.expect_get_alpha_substitute().returning({
        let other = other.clone();
        move |_pk| {
            Ok(wire_clowder::ConnectedMintResponse {
                mint: other.clone(),
                clowder: url::Url::from_str("https://clowder.example").unwrap(),
                node_id: test_pub_key(),
            })
        }
    });

    wlt = wallet_with_betas(
        wlt,
        vec![(b1, Arc::new(m1)), (b2, Arc::new(m2)), (b3, Arc::new(m3))],
    )
    .await;

    let res = wlt.read().await.mint_substitute().await.unwrap();
    assert_eq!(res, Some(substitute));
}

#[tokio::test]
async fn test_mint_substitute_majority_survives_a_dead_beta() {
    let ctx = wallet_ctx();
    let wlt = wallet(ctx).await;
    let substitute = url::Url::from_str("https://sub.example").unwrap();

    let mut betas = Vec::new();
    for (i, up) in [true, false, true].into_iter().enumerate() {
        let mut m = MockClowderMintConnector::new();
        let substitute = substitute.clone();
        m.expect_get_alpha_substitute().returning(move |_pk| {
            if up {
                Ok(wire_clowder::ConnectedMintResponse {
                    mint: substitute.clone(),
                    clowder: url::Url::from_str("https://clowder.example").unwrap(),
                    node_id: test_pub_key(),
                })
            } else {
                Err(bcr_common::client::mint::Error::Internal(
                    "beta down".into(),
                ))
            }
        });
        let url = url::Url::from_str(&format!("https://b{i}.example")).unwrap();
        betas.push((url, Arc::new(m) as Arc<dyn ClowderMintConnector>));
    }
    let wlt = wallet_with_betas(wlt, betas).await;

    let res = wlt.read().await.mint_substitute().await.unwrap();
    assert_eq!(res, Some(substitute));
}

#[tokio::test]
async fn test_offline_pay_by_token_errors_if_no_substitute() {
    let mut ctx = wallet_ctx();
    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);
    let wlt = wallet(ctx).await;
    wlt.write().await.beta_clients.clear();

    let err = wlt
        .read()
        .await
        .offline_pay_by_token(
            Uuid::new_v4(),
            CurrencyUnit::Sat,
            TransactionFees::default(),
            None,
            123,
        )
        .await
        .unwrap_err();

    match err {
        Error::NoSubstitute => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_pay_token_stores_tx_and_returns_token() {
    let mut ctx = wallet_ctx();

    let pid = Uuid::new_v4();

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));
    ctx.client
        .expect_mint_url()
        .times(2) // token creation + tx mint_url
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.debit
        .expect_unit()
        .times(2)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit
        .expect_send_proofs()
        .times(1)
        .returning(|_rid, _infos, _client, _safe| Ok(Default::default()));

    ctx.tx_repo
        .expect_store_tx()
        .times(1)
        .returning(|_tx| Ok(Uuid::new_v4()));

    let wlt = wallet(ctx).await;
    *wlt.read().await.current_payment.lock().await = Some(PayReference {
        request_id: pid,
        unit: CurrencyUnit::Sat,
        fees: TransactionFees::default(),
        ptype: WalletPaymentType::Token,
        memo: Some("memo".to_string()),
    });

    let (_txid, token) = wlt.read().await.pay(pid, 123).await.unwrap();

    assert!(token.is_some());
}

#[tokio::test]
async fn test_pay_errors_if_no_current_payment_reference() {
    let ctx = wallet_ctx();
    let wlt = wallet(ctx).await;

    let pid = Uuid::new_v4();

    let err = wlt.read().await.pay(pid, 123).await.unwrap_err();

    match err {
        Error::NoPrepareRef(id) => assert_eq!(id, pid),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_pay_errors_if_payment_reference_id_mismatch() {
    let ctx = wallet_ctx();
    let wlt = wallet(ctx).await;

    let prepared_pid = Uuid::new_v4();
    let actual_pid = Uuid::new_v4();

    *wlt.read().await.current_payment.lock().await = Some(PayReference {
        request_id: prepared_pid,
        unit: CurrencyUnit::Sat,
        fees: TransactionFees::default(),
        ptype: WalletPaymentType::Token,
        memo: None,
    });

    let err = wlt.read().await.pay(actual_pid, 123).await.unwrap_err();

    match err {
        Error::NoPrepareRef(id) => assert_eq!(id, actual_pid),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_pay_onchain() {
    let mut ctx = wallet_ctx();

    let pid = Uuid::new_v4();
    let tx_id = Uuid::new_v4();
    let quote_id = Uuid::new_v4();

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit
        .expect_pay_onchain_melt()
        .times(1)
        .returning(move |_request_id, _client| {
            Ok((
                bitcoin::Txid::all_zeros(),
                quote_id,
                HashMap::default(),
                Default::default(),
            ))
        });

    ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
        assert_eq!(tx.direction, TransactionDirection::Outgoing);
        assert_eq!(tx.amount, Amount::ZERO);
        assert_eq!(tx.fees.swap, Amount::ZERO);
        assert_eq!(tx.fees.melt, Amount::ZERO);
        assert_eq!(tx.fees.network, Amount::ZERO);
        assert_eq!(tx.unit, CurrencyUnit::Sat);
        assert_eq!(tx.tstamp, 123);
        assert_eq!(tx.memo, Some("melt memo".to_string()));
        assert_eq!(tx.quote_id, Some(quote_id));
        assert_eq!(tx.payment_type, PaymentType::OnChain);
        assert_eq!(tx.status, TransactionStatus::Settled);
        Ok(tx_id)
    });

    let wlt = wallet(ctx).await;

    *wlt.read().await.current_payment.lock().await = Some(PayReference {
        request_id: pid,
        unit: CurrencyUnit::Sat,
        fees: TransactionFees::default(),
        ptype: WalletPaymentType::OnChain,
        memo: Some("melt memo".to_string()),
    });

    let (res_tx_id, token) = wlt.read().await.pay(pid, 123).await.unwrap();

    assert_eq!(res_tx_id, tx_id);
    assert!(token.is_none());
}

#[tokio::test]
async fn protest_resolved_melt_stores_txid() {
    let mut ctx = wallet_ctx();
    let quote_id = Uuid::new_v4();
    let txid = bitcoin::Txid::from_byte_array([7u8; 32]);

    ctx.client
        .expect_mint_url()
        .return_const(url::Url::from_str("https://mint.example").unwrap());
    ctx.debit.expect_unit().returning(|| CurrencyUnit::Sat);
    ctx.debit
        .expect_protest_melt()
        .times(1)
        .returning(move |_| {
            Ok(MeltProtestResult {
                base: ProtestResult {
                    status: wire_common::ProtestStatus::Resolved,
                    result: Some((Amount::from(100u64), vec![])),
                },
                txid: Some(txid),
            })
        });
    ctx.tx_repo.expect_list_txs().returning(|| Ok(vec![]));
    ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
        assert_eq!(tx.btc_tx_id, Some(txid));
        assert_eq!(tx.quote_id, Some(quote_id));
        assert_eq!(tx.status, TransactionStatus::Settled);
        Ok(tx.id)
    });

    let wlt = wallet(ctx).await;
    let res = wlt.read().await.protest_melt(quote_id).await.unwrap();
    assert_eq!(res.status, wire_common::ProtestStatus::Resolved);
}

#[tokio::test]
async fn test_pay_contact() {
    let mut ctx = wallet_ctx();

    let pid = Uuid::new_v4();
    let tx_id = Uuid::new_v4();
    let contact_node_id = node_id(NODE_ID_1);
    let expected_node_id = contact_node_id.clone();

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client
        .expect_mint_url()
        .times(2)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.nostr_transport
        .expect_relays()
        .return_const(vec![RelayUrl::from_str("wss://test.example.com").unwrap()]);

    ctx.contact_repo
        .expect_get_contacts_by_node_id()
        .times(1)
        .returning(|_| {
            let mut contact = test_contact();
            contact.nostr_relays = vec![RelayUrl::from_str("wss://relay.theirs").unwrap()];
            Ok(vec![contact])
        });

    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .returning(|_, _| Ok(vec![]));

    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .returning(|target, _payload| {
            let profile = Nip19Profile::from_bech32(&target).unwrap();
            assert!(
                profile
                    .relays
                    .contains(&RelayUrl::from_str("wss://relay.theirs").unwrap())
            );
            assert!(
                profile
                    .relays
                    .contains(&RelayUrl::from_str("wss://test.example.com").unwrap())
            );
            Ok(EventId::from_byte_array([0u8; 32]))
        });

    ctx.debit
        .expect_send_proofs()
        .times(1)
        .returning(|_rid, _infos, _client, _swap| Ok(Default::default()));

    ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
        assert_eq!(tx.direction, TransactionDirection::Outgoing);
        assert_eq!(tx.amount, Amount::ZERO);
        assert_eq!(tx.fees.swap, Amount::ZERO);
        assert_eq!(tx.fees.melt, Amount::ZERO);
        assert_eq!(tx.fees.network, Amount::ZERO);
        assert_eq!(tx.unit, CurrencyUnit::Sat);
        assert_eq!(tx.tstamp, 123);
        assert_eq!(tx.memo, Some("contact memo".to_string()));
        assert_eq!(tx.payment_type, PaymentType::Contact,);
        assert_eq!(tx.status, TransactionStatus::Pending);
        assert_eq!(tx.contact_node_id, Some(expected_node_id.clone()));
        assert!(tx.nostr_event_id.is_some());
        Ok(tx_id)
    });

    let wlt = wallet(ctx).await;

    *wlt.read().await.current_payment.lock().await = Some(PayReference {
        request_id: pid,
        unit: CurrencyUnit::Sat,
        fees: TransactionFees::default(),
        ptype: WalletPaymentType::PaymentRequest {
            node_id: contact_node_id.clone(),
            payment_request_id: None,
        },
        memo: Some("contact memo".to_string()),
    });

    let (res_tx_id, token) = wlt.read().await.pay(pid, 123).await.unwrap();

    assert_eq!(res_tx_id, tx_id);
    assert!(token.is_none());
}

#[tokio::test]
async fn test_recover_pending_stale_proofs() {
    let mut ctx = wallet_ctx();

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.debit
        .expect_recover_pending_stale_proofs()
        .times(1)
        .returning(|_, _, _, _| Ok(Amount::from(10)));

    ctx.tx_repo.expect_list_txs().returning(|| Ok(vec![]));

    let wlt = wallet(ctx).await;
    let recovered = wlt
        .read()
        .await
        .recover_pending_stale_proofs()
        .await
        .unwrap();
    assert_eq!(recovered, Amount::from(10));
}

#[tokio::test]
async fn test_reclaim_tx_errors_if_transaction_cant_be_reclaimed() {
    let mut ctx = wallet_ctx();
    ctx.journal.expect_load().returning(|| Ok(HashMap::new()));
    let tx_id = Uuid::new_v4();

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    let tx = Transaction {
        status: TransactionStatus::Settled,
        ..reclaimable_tx(Amount::from(10))
    };

    ctx.tx_repo
        .expect_load_tx()
        .times(2)
        .returning(move |_| Ok(tx.clone()));

    let wlt = wallet(ctx).await;
    let err = wlt.read().await.reclaim_tx(tx_id).await.unwrap_err();

    match err {
        Error::TransactionCantBeReclaimed(id) => assert_eq!(id, tx_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_reclaim_tx_sets_settled_if_nothing_reclaimed() {
    let mut ctx = wallet_ctx();
    ctx.journal.expect_load().returning(|| Ok(HashMap::new()));
    let tx_id = Uuid::new_v4();
    let tx = reclaimable_tx(Amount::from(10));

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client
        .expect_post_check_state()
        .times(1)
        .returning(|_| Ok(vec![]));

    ctx.tx_repo
        .expect_load_tx()
        .times(2)
        .returning(move |_| Ok(tx.clone()));

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit
        .expect_reclaim_proofs()
        .times(1)
        .returning(|_, _, _, _| Ok(Amount::ZERO));

    ctx.tx_repo
        .expect_update_status()
        .times(1)
        .withf(|_, status| *status == TransactionStatus::Settled)
        .returning(|_, _| Ok(None));

    let wlt = wallet(ctx).await;
    let amount = wlt.read().await.reclaim_tx(tx_id).await.unwrap();

    assert_eq!(amount, Amount::ZERO);
}

#[tokio::test]
async fn test_reclaim_tx_creates_second_linked_tx() {
    let mut ctx = wallet_ctx();
    ctx.journal.expect_load().returning(|| Ok(HashMap::new()));
    let tx_id = Uuid::new_v4();
    let tx = reclaimable_tx(Amount::from(10));

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client
        .expect_post_check_state()
        .times(1)
        .returning(|_| Ok(vec![]));

    ctx.tx_repo
        .expect_load_tx()
        .times(2)
        .returning(move |_| Ok(tx.clone()));

    // store new tx
    ctx.tx_repo
        .expect_store_tx()
        .times(1)
        .returning(move |_| Ok(Uuid::new_v4()));

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit
        .expect_reclaim_proofs()
        .times(1)
        .returning(|_, _, _, _| Ok(Amount::from(7)));

    // update status of initial transaction to Settled
    ctx.tx_repo
        .expect_update_status()
        .times(1)
        .withf(|_, status| *status == TransactionStatus::Settled)
        .returning(|_, _| Ok(None));

    // link txs
    ctx.tx_repo
        .expect_link_txs()
        .times(1)
        .returning(|_, _, _| Ok(()));

    let wlt = wallet(ctx).await;
    let amount = wlt.read().await.reclaim_tx(tx_id).await.unwrap();

    assert_eq!(amount, Amount::from(7));
}

#[tokio::test]
async fn test_list_txs_all_sorts() {
    for sort in [
        TransactionSort::TimeAsc,
        TransactionSort::TimeDesc,
        TransactionSort::AmountAsc,
        TransactionSort::AmountDesc,
    ] {
        let txs = sample_txs();
        let expected = sort_expected(txs.clone(), sort);

        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_txs()
            .times(1)
            .returning(move || Ok(txs.clone()));

        let wlt = wallet(ctx).await;
        let ListTransactionsResult {
            txs, next_cursor, ..
        } = wlt
            .read()
            .await
            .list_txs(TransactionFilters::default(), sort, 20, None)
            .await
            .unwrap();

        assert_eq!(tx_ids(&txs), tx_ids(&expected));
        assert!(next_cursor.is_none());
    }
}

#[tokio::test]
async fn test_list_txs_paginates_without_overlap() {
    let txs = sample_txs();
    let expected = sort_expected(txs.clone(), TransactionSort::TimeDesc);

    let mut ctx = wallet_ctx();
    ctx.tx_repo
        .expect_list_txs()
        .times(3)
        .returning(move || Ok(txs.clone()));

    let wlt = wallet(ctx).await;

    let res1 = wlt
        .read()
        .await
        .list_txs(
            TransactionFilters::default(),
            TransactionSort::TimeDesc,
            2,
            None,
        )
        .await
        .unwrap();
    let page1 = res1.txs;
    let cursor1 = res1.next_cursor;

    assert_eq!(tx_ids(&page1), tx_ids(&expected[0..2]));
    assert_eq!(
        cursor1,
        Some(TransactionCursor::from_tx(
            page1.last().unwrap(),
            TransactionSort::TimeDesc
        ))
    );

    let res2 = wlt
        .read()
        .await
        .list_txs(
            TransactionFilters::default(),
            TransactionSort::TimeDesc,
            2,
            cursor1,
        )
        .await
        .unwrap();
    let page2 = res2.txs;
    let cursor2 = res2.next_cursor;

    assert_eq!(tx_ids(&page2), tx_ids(&expected[2..4]));
    assert_eq!(
        cursor2,
        Some(TransactionCursor::from_tx(
            page2.last().unwrap(),
            TransactionSort::TimeDesc
        ))
    );

    let res3 = wlt
        .read()
        .await
        .list_txs(
            TransactionFilters::default(),
            TransactionSort::TimeDesc,
            2,
            cursor2,
        )
        .await
        .unwrap();
    let page3 = res3.txs;
    let cursor3 = res3.next_cursor;

    assert_eq!(tx_ids(&page3), tx_ids(&expected[4..6]));
    assert!(cursor3.is_none());

    let all_ids = [tx_ids(&page1), tx_ids(&page2), tx_ids(&page3)].concat();
    let unique_ids: std::collections::BTreeSet<_> = all_ids.iter().collect();

    assert_eq!(all_ids.len(), expected.len());
    assert_eq!(unique_ids.len(), expected.len());
    assert_eq!(all_ids, tx_ids(&expected));
}

#[tokio::test]
async fn test_list_txs_applies_filters_and_inclusive_time_range() {
    let txs = sample_txs();
    let filters = TransactionFilters {
        payment_types: vec![PaymentType::Token],
        statuses: vec![TransactionStatus::Pending],
        direction: Some(TransactionDirection::Incoming),
        time_range: Some(TimeRange {
            from: Some(100),
            to: Some(300),
        }),
    };

    let mut expected: Vec<_> = txs
        .clone()
        .into_iter()
        .filter(|tx| {
            tx.payment_type == PaymentType::Token
                && tx.status == TransactionStatus::Pending
                && tx.direction == TransactionDirection::Incoming
                && tx.tstamp >= 100
                && tx.tstamp <= 300
        })
        .collect();

    expected = sort_expected(expected, TransactionSort::TimeDesc);

    let mut ctx = wallet_ctx();
    ctx.tx_repo
        .expect_list_txs()
        .times(1)
        .returning(move || Ok(txs.clone()));

    let wlt = wallet(ctx).await;
    let ListTransactionsResult {
        txs, next_cursor, ..
    } = wlt
        .read()
        .await
        .list_txs(filters, TransactionSort::TimeDesc, 2, None)
        .await
        .unwrap();

    assert_eq!(tx_ids(&txs), tx_ids(&expected));
    assert!(next_cursor.is_none());
    assert!(txs.iter().any(|tx| tx.tstamp == 300));
}

#[tokio::test]
async fn test_list_txs_next_cursor_uses_last_returned_tx() {
    let txs = sample_txs();
    let expected = sort_expected(txs.clone(), TransactionSort::TimeDesc);

    let mut ctx = wallet_ctx();
    ctx.tx_repo
        .expect_list_txs()
        .times(1)
        .returning(move || Ok(txs.clone()));

    let wlt = wallet(ctx).await;

    let ListTransactionsResult {
        txs, next_cursor, ..
    } = wlt
        .read()
        .await
        .list_txs(
            TransactionFilters::default(),
            TransactionSort::TimeDesc,
            5,
            None,
        )
        .await
        .unwrap();

    assert_eq!(tx_ids(&txs), tx_ids(&expected[0..5]));
    assert_eq!(
        next_cursor,
        Some(TransactionCursor::from_tx(
            txs.last().unwrap(),
            TransactionSort::TimeDesc,
        ))
    );

    assert_ne!(
        next_cursor,
        Some(TransactionCursor::from_tx(
            &expected[5],
            TransactionSort::TimeDesc,
        ))
    );
}

#[tokio::test]
async fn test_list_txs_cursor_transaction_does_not_need_to_exist() {
    let txs = sample_txs();
    let expected = sort_expected(txs.clone(), TransactionSort::TimeDesc);

    let cursor_tx = expected[5].clone();
    let cursor = TransactionCursor::from_tx(&cursor_tx, TransactionSort::TimeDesc);

    let mut txs_without_cursor = txs.clone();
    txs_without_cursor.retain(|tx| tx.id != cursor_tx.id);

    let expected_after_cursor: Vec<_> =
        sort_expected(txs_without_cursor.clone(), TransactionSort::TimeDesc)
            .into_iter()
            .filter(|tx| cursor.tx_is_after(tx))
            .take(6)
            .collect();

    let mut ctx = wallet_ctx();
    ctx.tx_repo
        .expect_list_txs()
        .times(1)
        .returning(move || Ok(txs_without_cursor.clone()));

    let wlt = wallet(ctx).await;
    let ListTransactionsResult { txs, .. } = wlt
        .read()
        .await
        .list_txs(
            TransactionFilters::default(),
            TransactionSort::TimeDesc,
            6,
            Some(cursor),
        )
        .await
        .unwrap();

    assert_eq!(tx_ids(&txs), tx_ids(&expected_after_cursor));
}

#[tokio::test]
async fn test_req_payment_from_contact() {
    let mut ctx = wallet_ctx();
    let relay = RelayUrl::from_str("wss://contact.example.com").unwrap();
    let contact = Contact {
        nostr_relays: vec![relay.clone()],
        ..test_contact()
    };
    let expected_npub = node_id(NODE_ID_1).npub();

    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .withf(move |recipient, _| {
            Nip19Profile::from_bech32(recipient)
                .is_ok_and(|p| p.public_key == expected_npub && p.relays == vec![relay.clone()])
        })
        .returning(|_, _| Ok(EventId::from_byte_array([0u8; 32])));
    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .returning(|_, _| Err(TransportError::Network("offline".to_string())));
    ctx.contact_repo
        .expect_get_contact()
        .times(2)
        .returning(move |_| Ok(Some(contact.clone())));
    ctx.payment_request_repo
        .expect_add_payment_request()
        .times(1)
        .returning(|_| Ok(()));
    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    let wlt = wallet(ctx).await;
    let _uuid = wlt
        .read()
        .await
        .request_payment_from_contact(
            Uuid::new_v4(),
            Amount::from(100),
            CurrencyUnit::Sat,
            None,
            None,
        )
        .await
        .expect("request payment from contact works");
}

async fn req_payment_from_node_id_with(
    fetched: fn() -> bcr_wallet_transport::error::Result<Vec<RelayUrl>>,
    sent: fn() -> bcr_wallet_transport::error::Result<EventId>,
    expected_relays: Vec<RelayUrl>,
    queued: usize,
) {
    let mut ctx = wallet_ctx();
    let target = node_id(NODE_ID_1);
    let own_relay = RelayUrl::from_str("wss://own.example.com").unwrap();

    ctx.nostr_transport
        .expect_relays()
        .times(1)
        .return_const(vec![own_relay.clone()]);
    let expected_npub = target.npub();
    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .withf(move |npub, relays| *npub == expected_npub && *relays == vec![own_relay.clone()])
        .returning(move |_, _| fetched());
    let expected_npub = target.npub();
    let is_target = move |recipient: &str| {
        Nip19Profile::from_bech32(recipient)
            .is_ok_and(|p| p.public_key == expected_npub && p.relays == expected_relays)
    };
    let is_sent_target = is_target.clone();
    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .withf(move |recipient, _| is_sent_target(recipient))
        .returning(move |_, _| sent());
    ctx.nostr_transport
        .expect_queue_retry_message()
        .times(queued)
        .withf(move |recipient, _| recipient.as_deref().is_some_and(&is_target))
        .returning(|_, _| Ok(()));
    let expected_node_id = target.clone();
    ctx.payment_request_repo
        .expect_add_payment_request()
        .times(1)
        .withf(move |req| {
            req.node_id.as_ref() == Some(&expected_node_id)
                && req.amount == Amount::from(100)
                && req.direction == PaymentRequestDirection::Outgoing
                && req.state == PaymentRequestState::Pending
        })
        .returning(|_| Ok(()));
    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    let wlt = wallet(ctx).await;
    wlt.read()
        .await
        .request_payment_from_node_id(target, Amount::from(100), CurrencyUnit::Sat, None, None)
        .await
        .expect("request payment from node id works");
}

fn their_relays() -> Vec<RelayUrl> {
    vec![RelayUrl::from_str("wss://their.example.com").unwrap()]
}

fn own_relays() -> Vec<RelayUrl> {
    vec![RelayUrl::from_str("wss://own.example.com").unwrap()]
}

fn sent_ok() -> bcr_wallet_transport::error::Result<EventId> {
    Ok(EventId::from_byte_array([0u8; 32]))
}

#[tokio::test]
async fn test_req_payment_from_node_id() {
    req_payment_from_node_id_with(|| Ok(their_relays()), sent_ok, their_relays(), 0).await;
}

#[tokio::test]
async fn test_req_payment_from_node_id_falls_back_to_own_relays_when_none_announced() {
    req_payment_from_node_id_with(|| Ok(vec![]), sent_ok, own_relays(), 0).await;
}

#[tokio::test]
async fn test_req_payment_from_node_id_falls_back_to_own_relays_when_lookup_fails() {
    req_payment_from_node_id_with(
        || Err(TransportError::Network("offline".to_string())),
        sent_ok,
        own_relays(),
        0,
    )
    .await;
}

#[tokio::test]
async fn test_req_payment_from_node_id_queues_when_send_fails() {
    req_payment_from_node_id_with(
        || Ok(their_relays()),
        || {
            Err(TransportError::NostrSendPrivateMsg(
                EventId::from_byte_array([1u8; 32]),
            ))
        },
        their_relays(),
        1,
    )
    .await;
}

#[tokio::test]
async fn test_req_payment_from_node_id_wrong_network() {
    let ctx = wallet_ctx();
    let wlt = wallet(ctx).await;
    let res = wlt
        .read()
        .await
        .request_payment_from_node_id(
            NodeId::new(test_other_pub_key(), bitcoin::Network::Bitcoin),
            Amount::from(100),
            CurrencyUnit::Sat,
            None,
            None,
        )
        .await;
    assert!(matches!(res, Err(Error::InvalidNetwork(..))));
}

#[tokio::test]
async fn test_list_payment_requests() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Pending,
    );

    ctx.payment_request_repo
        .expect_list_payment_requests()
        .times(1)
        .returning(move |direction, states| {
            assert_eq!(direction, PaymentRequestDirection::Incoming);
            assert_eq!(states, vec![PaymentRequestState::Pending]);
            Ok(vec![req.clone()])
        });

    let wlt = wallet(ctx).await;

    let res = wlt
        .read()
        .await
        .list_payment_requests(
            PaymentRequestDirection::Incoming,
            vec![PaymentRequestState::Pending],
        )
        .await
        .unwrap();

    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, req_id);
}

#[tokio::test]
async fn test_get_payment_request() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Pending,
    );

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |id| {
            assert_eq!(id, req_id);
            Ok(Some(req.clone()))
        });

    let wlt = wallet(ctx).await;

    let res = wlt.read().await.get_payment_request(req_id).await.unwrap();

    assert_eq!(res.unwrap().id, req_id);
}

#[tokio::test]
async fn test_add_payment_request() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Pending,
    );

    ctx.payment_request_repo
        .expect_add_payment_request()
        .times(1)
        .returning(move |stored| {
            assert_eq!(stored.id, req_id);
            assert_eq!(stored.direction, PaymentRequestDirection::Incoming);
            assert_eq!(stored.state, PaymentRequestState::Pending);
            Ok(())
        });

    let wlt = wallet(ctx).await;

    wlt.read().await.add_payment_request(req).await.unwrap();
}

#[tokio::test]
async fn test_reject_payment_request_sets_rejected() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Pending,
    );

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |id| {
            assert_eq!(id, req_id);
            Ok(Some(req.clone()))
        });

    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(move |id, transition| {
            assert_eq!(id, req_id);
            assert_eq!(transition.target_state, PaymentRequestState::Rejected);
            Ok(PaymentRequestTransitionOutcome::Applied)
        });
    ctx.contact_repo
        .expect_get_contacts_by_node_id()
        .times(1)
        .returning(|_| Ok(vec![]));
    ctx.nostr_transport
        .expect_relays()
        .return_const(Vec::<RelayUrl>::new());
    let fetched_relay = RelayUrl::from_str("wss://counterparty.example.com").unwrap();
    let expected_relays = vec![fetched_relay.clone()];
    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .returning(move |_, _| Ok(vec![fetched_relay.clone()]));
    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .returning(move |target, _payload| {
            let profile = Nip19Profile::from_bech32(&target).unwrap();
            assert_eq!(profile.public_key, node_id(NODE_ID_1).npub());
            assert_eq!(profile.relays, expected_relays);
            Ok(EventId::from_byte_array([0u8; 32]))
        });

    let wlt = wallet(ctx).await;

    wlt.read()
        .await
        .reject_payment_request(req_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_reject_payment_request_errors_if_already_resolved() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Rejected,
    );

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |_| Ok(Some(req.clone())));

    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(move |_, _| Ok(PaymentRequestTransitionOutcome::AlreadyApplied));

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .reject_payment_request(req_id)
        .await
        .unwrap_err();

    match err {
        Error::PaymentRequestInWrongState(id) => assert_eq!(id, req_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_reject_payment_request_errors_if_not_found() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |id| {
            assert_eq!(id, req_id);
            Ok(None)
        });

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .reject_payment_request(req_id)
        .await
        .unwrap_err();

    match err {
        Error::PaymentRequestNotFound(id) => assert_eq!(id, req_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_reject_payment_request_errors_if_outgoing() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |_| Ok(Some(req.clone())));

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .reject_payment_request(req_id)
        .await
        .unwrap_err();

    match err {
        Error::PaymentRequestInWrongState(id) => assert_eq!(id, req_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_cancel_payment_request_sets_canceled() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |id| {
            assert_eq!(id, req_id);
            Ok(Some(req.clone()))
        });

    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(move |id, transition| {
            assert_eq!(id, req_id);
            assert_eq!(transition.target_state, PaymentRequestState::Canceled);
            Ok(PaymentRequestTransitionOutcome::Applied)
        });
    ctx.contact_repo
        .expect_get_contacts_by_node_id()
        .times(1)
        .returning(|_| Ok(vec![]));
    ctx.nostr_transport
        .expect_relays()
        .return_const(Vec::<RelayUrl>::new());
    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .returning(|_, _| Ok(vec![]));
    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .returning(|_target, _payload| Ok(EventId::from_byte_array([0u8; 32])));

    let wlt = wallet(ctx).await;

    wlt.read()
        .await
        .cancel_payment_request(req_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_cancel_payment_request_errors_if_incoming() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Pending,
    );

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |_| Ok(Some(req.clone())));

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .cancel_payment_request(req_id)
        .await
        .unwrap_err();

    match err {
        Error::PaymentRequestInWrongState(id) => assert_eq!(id, req_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_mark_payment_request_as_paid_sets_paid_state() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let tx_id = Uuid::new_v4();

    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(move |id, transition| {
            assert_eq!(id, req_id);
            assert_eq!(transition.target_state, PaymentRequestState::Paid { tx_id });
            assert_eq!(transition.actor, Some(node_id(NODE_ID_1)));
            assert_eq!(transition.origin, PaymentRequestActionOrigin::Local);
            Ok(PaymentRequestTransitionOutcome::Applied)
        });

    let wlt = wallet(ctx).await;

    wlt.read()
        .await
        .mark_payment_request_as_paid(
            req_id,
            tx_id,
            node_id(NODE_ID_1),
            PaymentRequestActionOrigin::Local,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_mark_payment_request_as_paid_errors_if_not_found() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let tx_id = Uuid::new_v4();

    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(move |id, _| {
            Err(bcr_wallet_persistence::error::Error::PaymentRequestNotFound(id.to_string()))
        });

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .mark_payment_request_as_paid(
            req_id,
            tx_id,
            node_id(NODE_ID_1),
            PaymentRequestActionOrigin::Local,
        )
        .await
        .unwrap_err();

    match err {
        Error::PaymentRequestNotFound(id) => assert_eq!(id, req_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_mark_payment_request_as_paid_errors_if_conflicted() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let tx_id = Uuid::new_v4();

    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(move |_, _| Ok(PaymentRequestTransitionOutcome::Conflicted));

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .mark_payment_request_as_paid(
            req_id,
            tx_id,
            node_id(NODE_ID_1),
            PaymentRequestActionOrigin::Local,
        )
        .await
        .unwrap_err();

    match err {
        Error::PaymentRequestInWrongState(id) => assert_eq!(id, req_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_prepare_pay_payment_request_errors_if_not_found() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |id| {
            assert_eq!(id, req_id);
            Ok(None)
        });

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .prepare_pay_payment_request(req_id)
        .await
        .unwrap_err();

    match err {
        Error::PaymentRequestNotFound(id) => assert_eq!(id, req_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_prepare_pay_payment_request_errors_if_outgoing() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |_| Ok(Some(req.clone())));

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .prepare_pay_payment_request(req_id)
        .await
        .unwrap_err();

    match err {
        Error::PaymentRequestInWrongState(id) => assert_eq!(id, req_id),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_prepare_pay_payment_request_succeeds_without_contact() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Pending,
    );
    let requester = req.node_id.clone().unwrap();

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |_| Ok(Some(req.clone())));

    ctx.contact_repo.expect_get_contacts_by_node_id().never();
    ctx.contact_repo.expect_get_contact().never();

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.debit
        .expect_prepare_send()
        .times(1)
        .returning(|_amount, _infos| Ok(Default::default()));

    let wlt = wallet(ctx).await;

    wlt.read()
        .await
        .prepare_pay_payment_request(req_id)
        .await
        .unwrap();

    let binding = wlt.read().await;
    let pref_lock = binding.current_payment.lock().await;
    match &pref_lock.as_ref().unwrap().ptype {
        WalletPaymentType::PaymentRequest {
            node_id,
            payment_request_id,
        } => {
            assert_eq!(node_id, &requester);
            assert_eq!(payment_request_id, &Some(req_id));
        }
        other => panic!("unexpected payment type: {other:?}"),
    }
}

#[tokio::test]
async fn test_prepare_pay_to_contact_errors_if_contact_missing() {
    let mut ctx = wallet_ctx();
    let contact_id = Uuid::new_v4();

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.contact_repo
        .expect_get_contact()
        .times(1)
        .returning(|_| Ok(None));

    let wlt = wallet(ctx).await;

    let err = wlt
        .read()
        .await
        .prepare_pay_to_contact(contact_id, Amount::from(321), CurrencyUnit::Sat, None)
        .await
        .unwrap_err();

    match err {
        Error::ContactNotFound(id) => assert_eq!(id, contact_id.to_string()),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_prepare_pay_payment_request_errors_if_unit_mismatch() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = PaymentRequest {
        unit: CurrencyUnit::Usd,
        ..payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        )
    };

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |_| Ok(Some(req.clone())));
    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    let wlt = wallet(ctx).await;

    let res = wlt.read().await.prepare_pay_payment_request(req_id).await;

    assert!(matches!(res, Err(Error::InvalidCurrencyUnit(..))));
    assert!(wlt.read().await.current_payment.lock().await.is_none());
}

#[tokio::test]
async fn test_prepare_pay_payment_request_success_sets_contact_payment_reference() {
    let mut ctx = wallet_ctx();
    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Pending,
    );
    let requester = req.node_id.clone().unwrap();

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |id| {
            assert_eq!(id, req_id);
            Ok(Some(req.clone()))
        });

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.debit
        .expect_prepare_send()
        .times(1)
        .returning(|amount, _infos| {
            assert_eq!(amount, Amount::from(42));
            Ok(Default::default())
        });

    let wlt = wallet(ctx).await;

    let summary = wlt
        .read()
        .await
        .prepare_pay_payment_request(req_id)
        .await
        .unwrap();

    assert_eq!(summary.ptype, PaymentType::PaymentRequest);

    let binding = wlt.read().await;
    let pref_lock = binding.current_payment.lock().await;
    let p = pref_lock.as_ref().unwrap();

    match &p.ptype {
        WalletPaymentType::PaymentRequest {
            node_id,
            payment_request_id,
        } => {
            assert_eq!(node_id, &requester);
            assert_eq!(payment_request_id, &Some(req_id));
        }
        _ => panic!("unexpected payment type"),
    }

    assert_eq!(p.memo, Some("payment request memo".to_string()));
}

#[tokio::test]
async fn test_receive_proofs_stores_incoming_tx_for_wallet_mint() {
    let mut ctx = wallet_ctx();
    let tx_id = Uuid::new_v4();
    let ys = vec![test_cashu_pubkey(10)];

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client
        .expect_mint_url()
        .times(3)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit.expect_receive_proofs().times(1).returning(
        move |_client, _infos, proofs, _swap_config| {
            assert!(proofs.is_empty());
            Ok((Amount::ZERO, ys.clone()))
        },
    );

    ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
        assert_eq!(tx.direction, TransactionDirection::Incoming);
        assert_eq!(tx.amount, Amount::ZERO);
        assert_eq!(tx.fees.swap, Amount::ZERO);
        assert_eq!(tx.fees.melt, Amount::ZERO);
        assert_eq!(tx.fees.network, Amount::ZERO);
        assert_eq!(tx.unit, CurrencyUnit::Sat);
        assert_eq!(tx.tstamp, 123);
        assert_eq!(tx.memo, Some("memo".to_string()));
        Ok(tx_id)
    });

    let wlt = wallet(ctx).await;

    let res = wlt
        .read()
        .await
        .receive_proofs(
            vec![],
            CurrencyUnit::Sat,
            url::Url::from_str("https://mint.example").unwrap(),
            123,
            Some("memo".to_string()),
            PaymentType::Token,
            TransactionStatus::Settled,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(res, tx_id);
}

#[tokio::test]
async fn test_delete_shuts_down_and_deletes_repositories() {
    let mut ctx = wallet_ctx();

    ctx.nostr_transport
        .expect_shutdown()
        .times(1)
        .returning(|| ());

    ctx.debit.expect_delete().times(1).returning(|| Ok(()));

    ctx.tx_repo
        .expect_delete_repo()
        .times(1)
        .returning(|| Ok(()));

    ctx.nostr_repo
        .expect_delete_repo()
        .times(1)
        .returning(|| Ok(()));

    ctx.contact_repo
        .expect_delete_repo()
        .times(1)
        .returning(|| Ok(()));

    ctx.payment_request_repo
        .expect_delete_repo()
        .times(1)
        .returning(|| Ok(()));

    let wlt = wallet(ctx).await;

    wlt.read().await.delete().await.unwrap();
}

fn record_stored_payment_requests(
    ctx: &mut MockWalletCtx,
) -> tokio::sync::mpsc::UnboundedReceiver<Uuid> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.payment_request_repo
        .expect_add_payment_request()
        .returning(move |stored| {
            let _ = tx.send(stored.id);
            Ok(())
        });
    rx
}

fn contact_payment_request_event(
    id: Uuid,
    sender: nostr::key::PublicKey,
) -> bcr_wallet_transport::NostrWalletEvent {
    bcr_wallet_transport::NostrWalletEvent::ContactPaymentRequest {
        sender,
        payload: ContactPaymentRequestPayload {
            id,
            sender: node_id(NODE_ID_1),
            amount: Amount::from(1),
            unit: CurrencyUnit::Sat,
            memo: None,
            created_at: 123,
            mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
            deadline: None,
        },
        event_id: EventId::from_byte_array([1u8; 32]),
    }
}

fn publish_sentinel_payment_request(channel: &NostrEventChannel) -> Uuid {
    let id = Uuid::new_v4();
    channel.publish(contact_payment_request_event(id, node_id(NODE_ID_1).npub()));
    id
}

async fn assert_only_sentinel_processed(
    channel: &NostrEventChannel,
    mut stored: tokio::sync::mpsc::UnboundedReceiver<Uuid>,
) {
    let sentinel = publish_sentinel_payment_request(channel);
    let first = tokio::time::timeout(Duration::from_secs(1), stored.recv())
        .await
        .expect("listener should survive the forged event and process the sentinel")
        .expect("stored channel open");
    assert_eq!(first, sentinel, "forged event must not be stored");
}

#[tokio::test]
async fn test_start_nostr_event_listener_contact_payment() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let tx_id = Uuid::new_v4();
    let sender = node_id(NODE_ID_1);
    let sender_for_tx = sender.clone();
    let tx_id_for_store = tx_id;
    let tx_id_for_state = tx_id;
    let (processed_tx, processed_rx) = tokio::sync::oneshot::channel();
    let (marked_paid_tx, marked_paid_rx) = tokio::sync::oneshot::channel();

    let nostr_event_channel = ctx.nostr_event_channel.clone();

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);
    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));
    ctx.client
        .expect_mint_url()
        .return_const(url::Url::from_str("https://mint.example").unwrap());
    let ys = vec![test_cashu_pubkey(10)];
    let (_info, _keyset, paid_proofs) =
        test_keyset_and_proofs(&[Amount::from(32), Amount::from(8), Amount::from(2)]);
    ctx.debit.expect_receive_proofs().times(1).returning(
        move |_client, _infos, proofs, _swap_config| {
            assert_eq!(proofs.len(), 3);
            Ok((Amount::from(42), ys.clone()))
        },
    );
    ctx.tx_repo
        .expect_store_tx()
        .times(1)
        .return_once(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Incoming);
            assert_eq!(tx.amount, Amount::from(42));
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.memo, Some("payment request memo".to_string()));
            assert_eq!(tx.payment_type, PaymentType::PaymentRequest);
            assert_eq!(tx.status, TransactionStatus::Settled);
            assert_eq!(tx.payment_request_id, Some(req_id));
            assert_eq!(tx.contact_node_id, Some(sender_for_tx));
            processed_tx.send(()).expect("test receiver still alive");
            Ok(tx_id_for_store)
        });

    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );
    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .return_once(move |_| Ok(Some(req)));
    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .return_once(move |id, transition| {
            assert_eq!(id, req_id);
            assert_eq!(
                transition.target_state,
                PaymentRequestState::Paid {
                    tx_id: tx_id_for_state
                }
            );
            assert_eq!(transition.actor, Some(node_id(NODE_ID_1)));
            assert_eq!(
                transition.origin,
                PaymentRequestActionOrigin::Remote {
                    event_id: EventId::from_byte_array([0u8; 32]).to_string()
                }
            );
            marked_paid_tx.send(()).expect("test receiver still alive");
            Ok(PaymentRequestTransitionOutcome::Applied)
        });

    let wlt = wallet(ctx).await;
    Wallet::start_nostr_event_listener(wlt).await;

    let payload = ContactPaymentPayload {
        payment_request_id: Some(req_id),
        sender,
        proofs: paid_proofs,
        unit: CurrencyUnit::Sat,
        memo: Some("payment request memo".to_string()),
        created_at: 123,
        mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
    };

    nostr_event_channel.publish(bcr_wallet_transport::NostrWalletEvent::ContactPayment {
        sender: node_id(NODE_ID_1).npub(),
        payload,
        event_id: EventId::from_byte_array([0u8; 32]),
    });

    tokio::time::timeout(Duration::from_secs(1), processed_rx)
        .await
        .expect("contact payment event should be processed")
        .expect("contact payment processing signal should be sent");
    tokio::time::timeout(Duration::from_secs(1), marked_paid_rx)
        .await
        .expect("payment request should be marked as paid")
        .expect("paid-state signal should be sent");
}

async fn listener_pays_request(
    stored: Option<PaymentRequest>,
    paid: &[Amount],
    sender: NodeId,
) -> (usize, usize) {
    let (_info, _keyset, paid_proofs) = test_keyset_and_proofs(paid);
    let mut ctx = wallet_ctx();
    let req_id = stored.as_ref().map(|r| r.id).unwrap_or_else(Uuid::new_v4);
    let nostr_event_channel = ctx.nostr_event_channel.clone();
    let receives = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (looked_up_tx, looked_up_rx) = tokio::sync::oneshot::channel();

    ctx.debit.expect_unit().returning(|| CurrencyUnit::Sat);
    ctx.client
        .expect_get_mint_keysets()
        .returning(|| Ok(vec![]));
    ctx.client
        .expect_mint_url()
        .return_const(url::Url::from_str("https://mint.example").unwrap());
    let receives_cb = receives.clone();
    ctx.debit
        .expect_receive_proofs()
        .returning(move |_, _, _, _| {
            receives_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((Amount::ZERO, vec![test_cashu_pubkey(10)]))
        });
    ctx.tx_repo
        .expect_store_tx()
        .returning(|_| Ok(Uuid::new_v4()));
    ctx.payment_request_repo
        .expect_get_payment_request()
        .return_once(move |_| {
            looked_up_tx.send(()).expect("test receiver still alive");
            Ok(stored)
        });
    let writes_cb = writes.clone();
    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .returning(move |_, _| {
            writes_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(PaymentRequestTransitionOutcome::Applied)
        });

    let wlt = wallet(ctx).await;
    Wallet::start_nostr_event_listener(wlt).await;

    nostr_event_channel.publish(bcr_wallet_transport::NostrWalletEvent::ContactPayment {
        sender: sender.npub(),
        payload: ContactPaymentPayload {
            payment_request_id: Some(req_id),
            sender,
            proofs: paid_proofs,
            unit: CurrencyUnit::Sat,
            memo: None,
            created_at: 123,
            mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
        },
        event_id: EventId::from_byte_array([1u8; 32]),
    });

    tokio::time::timeout(Duration::from_secs(1), looked_up_rx)
        .await
        .expect("payment request should be looked up")
        .expect("lookup signal should be sent");
    tokio::time::sleep(Duration::from_millis(50)).await;
    (
        receives.load(std::sync::atomic::Ordering::SeqCst),
        writes.load(std::sync::atomic::Ordering::SeqCst),
    )
}

#[tokio::test]
async fn test_nostr_event_listener_payment_request_unknown_still_received() {
    assert_eq!(
        listener_pays_request(
            None,
            &[Amount::from(32), Amount::from(8), Amount::from(2)],
            node_id(NODE_ID_1)
        )
        .await,
        (1, 0)
    );
}

#[tokio::test]
async fn test_nostr_event_listener_payment_request_incoming_not_marked() {
    let req = payment_request_with(
        Uuid::new_v4(),
        PaymentRequestDirection::Incoming,
        PaymentRequestState::Pending,
    );
    assert_eq!(
        listener_pays_request(
            Some(req),
            &[Amount::from(32), Amount::from(8), Amount::from(2)],
            node_id(NODE_ID_1)
        )
        .await,
        (1, 0)
    );
}

#[tokio::test]
async fn test_nostr_event_listener_payment_request_already_paid_kept() {
    let req = payment_request_with(
        Uuid::new_v4(),
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Paid {
            tx_id: Uuid::new_v4(),
        },
    );
    assert_eq!(
        listener_pays_request(
            Some(req),
            &[Amount::from(32), Amount::from(8), Amount::from(2)],
            node_id(NODE_ID_1)
        )
        .await,
        (1, 0)
    );
}

#[tokio::test]
async fn test_nostr_event_listener_payment_request_underpaid_not_marked() {
    let req = payment_request_with(
        Uuid::new_v4(),
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );
    let paid = [Amount::from(32), Amount::from(8)];
    assert_eq!(
        listener_pays_request(Some(req), &paid, node_id(NODE_ID_1)).await,
        (1, 0)
    );
}

#[tokio::test]
async fn test_nostr_event_listener_payment_request_wrong_sender_not_marked() {
    let req = payment_request_with(
        Uuid::new_v4(),
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );
    assert_eq!(
        listener_pays_request(Some(req), &[Amount::from(64)], node_id(NODE_ID_2)).await,
        (1, 0)
    );
}

#[tokio::test]
async fn test_nostr_event_listener_payment_request_shared_any_sender_marked() {
    let mut req = payment_request_with(
        Uuid::new_v4(),
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );
    req.node_id = None;
    assert_eq!(
        listener_pays_request(Some(req), &[Amount::from(64)], node_id(NODE_ID_2)).await,
        (1, 1)
    );
}

#[tokio::test]
async fn test_nostr_event_listener_payment_request_outgoing_pending_marked() {
    let req = payment_request_with(
        Uuid::new_v4(),
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );
    assert_eq!(
        listener_pays_request(
            Some(req),
            &[Amount::from(32), Amount::from(8), Amount::from(2)],
            node_id(NODE_ID_1)
        )
        .await,
        (1, 1)
    );
}

#[tokio::test]
async fn test_start_nostr_event_listener_contact_payment_request() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let sender = node_id(NODE_ID_1);
    let sender_for_assert = sender.clone();
    let (stored_tx, stored_rx) = tokio::sync::oneshot::channel();

    let nostr_event_channel = ctx.nostr_event_channel.clone();

    ctx.payment_request_repo
        .expect_add_payment_request()
        .times(1)
        .return_once(move |stored| {
            assert_eq!(stored.id, req_id);
            assert_eq!(stored.node_id, Some(sender_for_assert));
            assert_eq!(stored.amount, Amount::from(42));
            assert_eq!(stored.unit, CurrencyUnit::Sat);
            assert_eq!(stored.description, Some("payment request memo".to_string()));
            assert_eq!(stored.direction, PaymentRequestDirection::Incoming);
            assert_eq!(stored.state, PaymentRequestState::Pending);

            stored_tx.send(()).expect("test receiver still alive");
            Ok(())
        });

    let wlt = wallet(ctx).await;
    Wallet::start_nostr_event_listener(wlt).await;

    let payload = ContactPaymentRequestPayload {
        id: req_id,
        sender,
        amount: Amount::from(42),
        unit: CurrencyUnit::Sat,
        memo: Some("payment request memo".to_string()),
        created_at: 123,
        mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
        deadline: None,
    };

    nostr_event_channel.publish(
        bcr_wallet_transport::NostrWalletEvent::ContactPaymentRequest {
            sender: node_id(NODE_ID_1).npub(),
            payload,
            event_id: EventId::from_byte_array([0u8; 32]),
        },
    );

    tokio::time::timeout(Duration::from_secs(1), stored_rx)
        .await
        .expect("contact payment request event should be processed")
        .expect("payment-request storage signal should be sent");
}

#[tokio::test]
async fn test_start_nostr_event_listener_rejects_forged_sender_on_contact_payment() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let nostr_event_channel = ctx.nostr_event_channel.clone();
    let stored = record_stored_payment_requests(&mut ctx);
    let wlt = wallet(ctx).await;
    Wallet::start_nostr_event_listener(wlt).await;

    let payload = ContactPaymentPayload {
        payment_request_id: Some(req_id),
        sender: node_id(NODE_ID_1),
        proofs: vec![],
        unit: CurrencyUnit::Sat,
        memo: None,
        created_at: 123,
        mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
    };

    nostr_event_channel.publish(bcr_wallet_transport::NostrWalletEvent::ContactPayment {
        sender: node_id(NODE_ID_2).npub(),
        payload,
        event_id: EventId::from_byte_array([0u8; 32]),
    });

    assert_only_sentinel_processed(&nostr_event_channel, stored).await;
}

#[tokio::test]
async fn test_start_nostr_event_listener_rejects_forged_sender_on_contact_payment_request() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let nostr_event_channel = ctx.nostr_event_channel.clone();
    let stored = record_stored_payment_requests(&mut ctx);
    let wlt = wallet(ctx).await;
    Wallet::start_nostr_event_listener(wlt).await;

    let payload = ContactPaymentRequestPayload {
        id: req_id,
        sender: node_id(NODE_ID_1),
        amount: Amount::from(42),
        unit: CurrencyUnit::Sat,
        memo: None,
        created_at: 123,
        mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
        deadline: None,
    };

    nostr_event_channel.publish(
        bcr_wallet_transport::NostrWalletEvent::ContactPaymentRequest {
            sender: node_id(NODE_ID_2).npub(),
            payload,
            event_id: EventId::from_byte_array([0u8; 32]),
        },
    );

    assert_only_sentinel_processed(&nostr_event_channel, stored).await;
}

#[tokio::test]
async fn test_payment_request_action_cancel_is_applied_for_matching_counterparty() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let actor = node_id(NODE_ID_1);
    let (applied_tx, applied_rx) = tokio::sync::oneshot::channel();
    let nostr_event_channel = ctx.nostr_event_channel.clone();

    ctx.payment_request_repo
        .expect_apply_remote_payment_request_transition()
        .times(1)
        .return_once(move |id, counterparty, direction, transition| {
            assert_eq!(id, req_id);
            assert_eq!(counterparty, node_id(NODE_ID_1));
            assert_eq!(direction, PaymentRequestDirection::Incoming);
            assert_eq!(transition.target_state, PaymentRequestState::Canceled);
            assert_eq!(transition.actor, Some(node_id(NODE_ID_1)));
            assert_eq!(transition.at, 123);
            assert_eq!(
                transition.origin,
                PaymentRequestActionOrigin::Remote {
                    event_id: EventId::from_byte_array([0u8; 32]).to_string()
                }
            );
            assert_eq!(transition.reason, Some("no longer needed".to_string()));
            applied_tx.send(()).expect("test receiver still alive");
            Ok(Some(PaymentRequestTransitionOutcome::Applied))
        });

    let wlt = wallet(ctx).await;
    Wallet::start_nostr_event_listener(wlt).await;

    let payload = PaymentRequestActionPayload {
        payment_request_id: req_id,
        action: PaymentRequestActionKind::Cancel,
        actor: actor.clone(),
        acted_at: 123,
        reason: Some("no longer needed".to_string()),
    };
    nostr_event_channel.publish(
        bcr_wallet_transport::NostrWalletEvent::PaymentRequestAction {
            sender: actor.npub(),
            payload,
            event_id: EventId::from_byte_array([0u8; 32]),
        },
    );

    tokio::time::timeout(Duration::from_secs(1), applied_rx)
        .await
        .expect("payment request action should be processed")
        .expect("applied signal should be sent");
}

#[tokio::test]
async fn test_apply_remote_payment_request_action_reports_whether_a_request_was_updated() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let actor = node_id(NODE_ID_1);
    let mut outcomes = vec![Some(PaymentRequestTransitionOutcome::Applied), None];
    ctx.payment_request_repo
        .expect_apply_remote_payment_request_transition()
        .times(2)
        .returning(move |_, _, _, _| Ok(outcomes.pop().flatten()));
    let wlt = wallet(ctx).await;
    let payload = PaymentRequestActionPayload {
        payment_request_id: req_id,
        action: PaymentRequestActionKind::Cancel,
        actor: actor.clone(),
        acted_at: 123,
        reason: None,
    };
    let event_id = EventId::from_byte_array([0u8; 32]);
    let apply = |sender| {
        let wlt = wlt.clone();
        let payload = payload.clone();
        async move {
            wlt.read()
                .await
                .apply_remote_payment_request_action(payload, event_id, sender)
                .await
                .unwrap()
        }
    };

    assert!(!apply(node_id(NODE_ID_2).npub()).await);
    assert!(!apply(actor.npub()).await);
    assert!(apply(actor.npub()).await);
}

#[tokio::test]
async fn test_handle_payment_request_event_notifies_stored_or_known_requests_only() {
    let mut ctx = wallet_ctx();
    let mut results = vec![
        Err(
            bcr_wallet_persistence::error::Error::PaymentRequestNotFound(
                "store failed".to_string(),
            ),
        ),
        Err(bcr_wallet_persistence::error::Error::PaymentRequestAlreadyExists("known".to_string())),
        Ok(()),
    ];
    ctx.payment_request_repo
        .expect_add_payment_request()
        .times(3)
        .returning(move |_| results.pop().unwrap());
    let wlt = wallet(ctx).await;
    let wlt = wlt.read().await;
    let id = Uuid::new_v4();
    let authentic = || contact_payment_request_event(id, node_id(NODE_ID_1).npub());

    assert_eq!(
        wlt.handle_payment_request_event(authentic()).await,
        Some(id)
    );
    assert_eq!(
        wlt.handle_payment_request_event(authentic()).await,
        Some(id)
    );
    assert_eq!(wlt.handle_payment_request_event(authentic()).await, None);
    assert_eq!(
        wlt.handle_payment_request_event(contact_payment_request_event(
            id,
            node_id(NODE_ID_2).npub()
        ))
        .await,
        None
    );
}

#[tokio::test]
async fn test_handle_payment_request_event_notifies_visible_actions_only() {
    let mut ctx = wallet_ctx();
    let mut results = vec![
        Err(
            bcr_wallet_persistence::error::Error::PaymentRequestNotFound(
                "store failed".to_string(),
            ),
        ),
        Ok(None),
        Ok(Some(PaymentRequestTransitionOutcome::AlreadyApplied)),
    ];
    ctx.payment_request_repo
        .expect_apply_remote_payment_request_transition()
        .times(3)
        .returning(move |_, _, _, _| results.pop().unwrap());
    let wlt = wallet(ctx).await;
    let wlt = wlt.read().await;
    let id = Uuid::new_v4();
    let action = || bcr_wallet_transport::NostrWalletEvent::PaymentRequestAction {
        sender: node_id(NODE_ID_1).npub(),
        payload: PaymentRequestActionPayload {
            payment_request_id: id,
            action: PaymentRequestActionKind::Reject,
            actor: node_id(NODE_ID_1),
            acted_at: 123,
            reason: None,
        },
        event_id: EventId::from_byte_array([0u8; 32]),
    };

    assert_eq!(wlt.handle_payment_request_event(action()).await, Some(id));
    assert_eq!(wlt.handle_payment_request_event(action()).await, None);
    assert_eq!(wlt.handle_payment_request_event(action()).await, None);
    assert_eq!(
        wlt.handle_payment_request_event(bcr_wallet_transport::NostrWalletEvent::ContactPayment {
            sender: node_id(NODE_ID_1).npub(),
            payload: ContactPaymentPayload {
                payment_request_id: Some(id),
                sender: node_id(NODE_ID_1),
                proofs: vec![],
                unit: CurrencyUnit::Sat,
                memo: None,
                created_at: 123,
                mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
            },
            event_id: EventId::from_byte_array([0u8; 32]),
        })
        .await,
        None
    );
}

#[tokio::test]
async fn test_cancel_payment_request_looks_up_relays_via_the_contact_and_survives_a_failed_retry_queue()
 {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let req = payment_request_with(
        req_id,
        PaymentRequestDirection::Outgoing,
        PaymentRequestState::Pending,
    );
    let contact_relay = RelayUrl::from_str("wss://contact.example.com").unwrap();
    let mut contact = test_contact();
    contact.nostr_relays = vec![contact_relay.clone()];

    ctx.payment_request_repo
        .expect_get_payment_request()
        .times(1)
        .returning(move |_| Ok(Some(req.clone())));
    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(|_, _| Ok(PaymentRequestTransitionOutcome::Applied));
    ctx.contact_repo
        .expect_get_contacts_by_node_id()
        .times(1)
        .returning(move |_| Ok(vec![contact.clone()]));
    ctx.nostr_transport.expect_relays().return_const(vec![]);
    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .returning(move |_, relays| {
            assert_eq!(relays, vec![contact_relay.clone()]);
            Ok(vec![])
        });
    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .returning(|_, _| {
            Err(bcr_wallet_transport::error::Error::Network(
                "offline".to_string(),
            ))
        });
    ctx.nostr_transport
        .expect_queue_retry_message()
        .times(1)
        .returning(|_, _| {
            Err(bcr_wallet_transport::error::Error::Network(
                "queue unavailable".to_string(),
            ))
        });

    let wlt = wallet(ctx).await;

    wlt.read()
        .await
        .cancel_payment_request(req_id)
        .await
        .unwrap();
}

#[test]
fn test_is_authentic_sender_requires_network_and_seal_signer() {
    let claimed = node_id(NODE_ID_1);
    assert!(is_authentic_sender(
        &claimed,
        &claimed.npub(),
        claimed.network()
    ));
    assert!(!is_authentic_sender(
        &claimed,
        &node_id(NODE_ID_2).npub(),
        claimed.network()
    ));
    assert!(!is_authentic_sender(
        &claimed,
        &claimed.npub(),
        bitcoin::Network::Bitcoin
    ));
}

#[tokio::test]
async fn test_payment_request_action_rejects_forged_actor() {
    let mut ctx = wallet_ctx();
    let req_id = Uuid::new_v4();
    let nostr_event_channel = ctx.nostr_event_channel.clone();
    let stored = record_stored_payment_requests(&mut ctx);
    let wlt = wallet(ctx).await;
    Wallet::start_nostr_event_listener(wlt).await;

    let payload = PaymentRequestActionPayload {
        payment_request_id: req_id,
        action: PaymentRequestActionKind::Cancel,
        actor: node_id(NODE_ID_1),
        acted_at: 123,
        reason: None,
    };
    nostr_event_channel.publish(
        bcr_wallet_transport::NostrWalletEvent::PaymentRequestAction {
            sender: node_id(NODE_ID_2).npub(),
            payload,
            event_id: EventId::from_byte_array([0u8; 32]),
        },
    );

    assert_only_sentinel_processed(&nostr_event_channel, stored).await;
}

#[tokio::test]
async fn test_reject_payment_request_queues_retry_on_any_send_error() {
    let send_errors = [
        bcr_wallet_transport::error::Error::NostrSendPrivateMsg(EventId::from_byte_array(
            [0u8; 32],
        )),
        bcr_wallet_transport::error::Error::Network(
            "all relays failed to accept the event".to_string(),
        ),
    ];
    for send_error in send_errors {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |_| Ok(Some(req.clone())));
        ctx.payment_request_repo
            .expect_apply_payment_request_transition()
            .times(1)
            .returning(|_, _| Ok(PaymentRequestTransitionOutcome::Applied));
        ctx.contact_repo
            .expect_get_contacts_by_node_id()
            .times(1)
            .returning(|_| Ok(vec![]));
        ctx.nostr_transport
            .expect_relays()
            .return_const(Vec::<RelayUrl>::new());
        ctx.nostr_transport
            .expect_fetch_relay_list()
            .times(1)
            .returning(|_, _| Ok(vec![]));
        ctx.nostr_transport
            .expect_send_private_msg()
            .times(1)
            .return_once(move |_target, _payload| Err(send_error));
        ctx.nostr_transport
            .expect_queue_retry_message()
            .times(1)
            .returning(|_recipient, _payload| Ok(()));

        let wlt = wallet(ctx).await;

        wlt.read()
            .await
            .reject_payment_request(req_id)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn test_online_exchange_returns_input_if_alpha_is_wallet_mint() {
    let mut ctx = wallet_ctx();

    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    let wlt = wallet(ctx).await;

    let alpha_client = MockClowderMintConnector::new();
    let alpha_url = url::Url::from_str("https://mint.example").unwrap();

    let (res, hash_lock) = wlt
        .read()
        .await
        .online_exchange(vec![], alpha_url, Arc::new(alpha_client), vec![], 123)
        .await
        .unwrap();

    assert!(res.is_empty());
    assert!(hash_lock.is_none());
}

#[tokio::test]
async fn test_receive_token_errors_on_invalid_intermint_clowder_path() {
    let mut ctx = wallet_ctx();

    let token_mint = url::Url::from_str("https://other-mint.example").unwrap();

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.client
        .expect_post_clowder_path()
        .times(1)
        .withf({
            let token_mint = token_mint.clone();
            move |mint| *mint == token_mint
        })
        .returning(|mint| {
            Ok(wire_clowder::ConnectedMintsResponse {
                mints: vec![wire_clowder::ConnectedMintResponse {
                    mint,
                    clowder: url::Url::from_str("https://clowder.example").unwrap(),
                    node_id: test_pub_key(),
                }],
            })
        });

    let wlt = wallet(ctx).await;

    let token = Token::new_bitcr(
        to_mint_url(&token_mint),
        vec![],
        Some("intermint token".to_string()),
        CurrencyUnit::Sat,
    );

    let err = wlt
        .read()
        .await
        .receive_token(token, 123)
        .await
        .unwrap_err();

    match err {
        Error::InvalidClowderPath => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_receive_token_v4_same_mint_success() {
    let mut ctx = wallet_ctx();

    let tx_id = Uuid::new_v4();
    let mint_url = url::Url::from_str("https://mint.example").unwrap();

    let (info, _keyset, proofs) = test_keyset_and_proofs(&[Amount::from(8), Amount::from(16)]);
    let k_infos = HashMap::from([(info.id, info.clone())]);

    let expected_ys: Vec<_> = proofs
        .iter()
        .map(|p| p.y().expect("valid proof y"))
        .collect();

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(move || Ok(k_infos.values().cloned().collect()));

    ctx.client
        .expect_mint_url()
        .times(4)
        .return_const(mint_url.clone());

    ctx.debit
        .expect_unit()
        .times(3)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit.expect_receive_proofs().times(1).return_once(
        move |_client, keysets_info, received_proofs, _swap_config| {
            assert!(keysets_info.contains_key(&info.id));
            assert_eq!(received_proofs.len(), 2);
            assert_eq!(received_proofs.total_amount().unwrap(), Amount::from(24));
            Ok((Amount::from(24), expected_ys))
        },
    );
    ctx.tx_repo
        .expect_store_tx()
        .times(1)
        .return_once(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Incoming);
            assert_eq!(tx.amount, Amount::from(24));
            assert_eq!(tx.fees.swap, Amount::ZERO);
            assert_eq!(tx.fees.melt, Amount::ZERO);
            assert_eq!(tx.fees.network, Amount::ZERO);
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.tstamp, 123);
            assert_eq!(tx.memo, Some("token memo".to_string()));
            assert_eq!(tx.payment_type, PaymentType::Token);
            assert_eq!(tx.status, TransactionStatus::Settled);
            Ok(tx_id)
        });

    let token = Token::new_bitcr(
        to_mint_url(&mint_url),
        proofs.into_iter().map(|p| p.into()).collect(),
        Some("token memo".to_string()),
        CurrencyUnit::Sat,
    );

    let wlt = wallet(ctx).await;

    let res = wlt.read().await.receive_token(token, 123).await.unwrap();

    assert_eq!(res, tx_id);
}

#[tokio::test]
async fn test_receive_token_v5_same_mint_success() {
    let mut ctx = wallet_ctx();

    let tx_id = Uuid::new_v4();
    let mint_url = url::Url::from_str("https://mint.example").unwrap();

    let (info, _keyset, proofs) = test_keyset_and_proofs(&[Amount::from(8), Amount::from(16)]);
    let k_infos = HashMap::from([(info.id, info.clone())]);

    let expected_ys: Vec<_> = proofs
        .iter()
        .map(|p| p.y().expect("valid proof y"))
        .collect();

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(move || Ok(k_infos.values().cloned().collect()));

    ctx.client
        .expect_mint_url()
        .times(5)
        .return_const(mint_url.clone());

    ctx.debit
        .expect_unit()
        .times(3)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit.expect_receive_proofs().times(1).return_once(
        move |_client, keysets_info, received_proofs, _swap_config| {
            assert!(keysets_info.contains_key(&info.id));
            assert_eq!(received_proofs.len(), 2);
            assert_eq!(received_proofs.total_amount().unwrap(), Amount::from(24));
            Ok((Amount::from(24), expected_ys))
        },
    );
    ctx.tx_repo
        .expect_store_tx()
        .times(1)
        .return_once(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Incoming);
            assert_eq!(tx.amount, Amount::from(24));
            assert_eq!(tx.fees.swap, Amount::ZERO);
            assert_eq!(tx.fees.melt, Amount::ZERO);
            assert_eq!(tx.fees.network, Amount::ZERO);
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.tstamp, 123);
            assert_eq!(tx.memo, Some("token memo".to_string()));
            assert_eq!(tx.payment_type, PaymentType::Token);
            assert_eq!(tx.status, TransactionStatus::Settled);
            Ok(tx_id)
        });

    let token = Token::BitcrV5(
        BitcrTokenV5::new(
            NodeId::new(test_pub_key(), bitcoin::Network::Testnet),
            CurrencyUnit::Sat,
            proofs.into_iter().map(|p| p.into()).collect(),
        )
        .with_memo("token memo".to_string()),
    );

    let wlt = wallet(ctx).await;

    let res = wlt.read().await.receive_token(token, 123).await.unwrap();

    assert_eq!(res, tx_id);
}

#[tokio::test]
async fn test_online_exchange_success_with_valid_proofs() {
    let mut ctx = wallet_ctx();

    let wallet_mint = url::Url::from_str("https://wallet-mint.example").unwrap();
    let alpha_mint = url::Url::from_str("https://alpha-mint.example").unwrap();
    let alpha_beta_url = url::Url::from_str("https://alpha-beta.example").unwrap();

    let (info, alpha_keyset) = core_tests::generate_random_ecash_keyset();
    let kid = info.id;
    let k_infos = test_kinfos(info);
    let alpha_proofs = core_tests::generate_random_ecash_proofs(&alpha_keyset, &[Amount::from(8)]);

    let mut alpha_client = MockClowderMintConnector::new();
    let mut alpha_beta = MockClowderMintConnector::new();
    setup_attestation_mock(&mut alpha_beta);

    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(wallet_mint.clone());

    ctx.client
        .expect_post_online_exchange()
        .times(1)
        .withf(|locked_proofs, exchange_path| locked_proofs.len() == 1 && exchange_path.len() == 3)
        .returning(|locked_proofs, _exchange_path| Ok(locked_proofs));

    let url_clone = alpha_beta_url.clone();
    alpha_client
        .expect_get_clowder_betas()
        .times(1)
        .returning(move || {
            Ok(vec![ClowderBeta {
                url: url_clone.clone(),
                clowder_id: bcr_common::core::generate_random_keypair().public_key(),
            }])
        });

    alpha_client
        .expect_get_mint_keysets()
        .times(1)
        .returning(move || Ok(k_infos.values().cloned().collect()));
    let alpha_keyset_for_lookup = alpha_keyset.clone();
    alpha_client
        .expect_get_mint_keyset()
        .times(1)
        .with(mockall::predicate::eq(kid))
        .returning(move |_| {
            Ok(bcr_wallet_core::util::to_keyset(
                &alpha_keyset_for_lookup,
                None,
            ))
        });

    let alpha_keyset_for_lock = alpha_keyset.clone();
    ctx.debit
        .expect_lock_exchange()
        .times(1)
        .returning(move |record, _, _, _, _| {
            assert!(record.locked_proofs.is_none());
            Ok(core_tests::generate_random_ecash_proofs(
                &alpha_keyset_for_lock,
                &[Amount::from(8)],
            ))
        });
    ctx.debit.expect_delete_exchange_record().times(0);

    let mut wlt = wallet(ctx).await;

    let arc_alpha_beta = Arc::new(alpha_beta);
    wlt = wallet_with_betas(wlt, vec![(alpha_beta_url, arc_alpha_beta.clone())]).await;
    wlt.write().await.client_factory = Box::new(move |url| {
        assert_eq!(
            url,
            url::Url::from_str("https://alpha-beta.example").unwrap()
        );
        arc_alpha_beta.clone()
    });

    let path = vec![
        wire_clowder::ConnectedMintResponse {
            mint: alpha_mint.clone(),
            clowder: url::Url::from_str("https://clowder-alpha.example").unwrap(),
            node_id: test_pub_key(),
        },
        wire_clowder::ConnectedMintResponse {
            mint: wallet_mint.clone(),
            clowder: url::Url::from_str("https://clowder-wallet.example").unwrap(),
            node_id: test_pub_key(),
        },
    ];
    let now = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
    let (res, hash_lock) = wlt
        .read()
        .await
        .online_exchange(alpha_proofs, alpha_mint, Arc::new(alpha_client), path, now)
        .await
        .unwrap();

    assert!(hash_lock.is_some());
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].amount, Amount::from(8));
    assert!(res[0].witness.is_some());
}

fn add_test_dleqs(proofs: &mut [cashu::Proof]) {
    for proof in proofs {
        proof.dleq = Some(cashu::nut12::ProofDleq {
            e: cashu::SecretKey::generate(),
            s: cashu::SecretKey::generate(),
            r: cashu::SecretKey::generate(),
        });
    }
}
fn exchange_locked(
    proofs: Vec<Proof>,
    hash_locks: &[Sha256],
    wallet_pk: secp256k1::PublicKey,
    issuer: secp256k1::PublicKey,
) -> Vec<Proof> {
    exchange_locked_for(proofs, hash_locks, wallet_pk, issuer, 3600)
}

fn exchange_locked_for(
    mut proofs: Vec<Proof>,
    hash_locks: &[Sha256],
    wallet_pk: secp256k1::PublicKey,
    issuer: secp256k1::PublicKey,
    lock_secs: u64,
) -> Vec<Proof> {
    let locktime = time::OffsetDateTime::now_utc().unix_timestamp() as u64 + lock_secs;
    for (proof, hash_lock) in proofs.iter_mut().zip(hash_locks) {
        let htlc =
            htlc::exchange_htlc(*hash_lock, locktime, wallet_pk.into(), issuer.into()).unwrap();
        proof.secret = bcr_common::core::signature::offline_htlc_secret(htlc).unwrap();
    }
    proofs
}

async fn offline_exchange_issuing(
    issue: impl FnOnce(Vec<Proof>, &[Sha256], secp256k1::PublicKey, secp256k1::PublicKey) -> Vec<Proof>
    + Send
    + 'static,
) -> Result<Vec<Proof>> {
    let mut ctx = wallet_ctx();
    ctx.debit
        .expect_exchange_key()
        .returning(|proofs| crate::pocket::exchange_key(&zero_seed(), proofs));
    let wlt = wallet(ctx).await;

    let (_alpha_info, alpha_keyset) = core_tests::generate_random_ecash_keyset();
    let mut alpha_proofs =
        core_tests::generate_random_ecash_proofs(&alpha_keyset, &[Amount::from(8)]);
    add_test_dleqs(&mut alpha_proofs);

    let (_beta_info, beta_keyset) = core_tests::generate_random_ecash_keyset();
    let mut beta_proofs =
        core_tests::generate_random_ecash_proofs(&beta_keyset, &[Amount::from(8)]);
    add_test_dleqs(&mut beta_proofs);

    let mut substitute = MockClowderMintConnector::new();

    substitute
        .expect_post_offline_exchange()
        .times(1)
        .withf(
            |fingerprints, hash_locks, _wallet_pk, _wallet_signature, substitute_clowder_id| {
                fingerprints.len() == 1
                    && hash_locks.len() == 1
                    && *substitute_clowder_id == test_pub_key()
            },
        )
        .return_once(
            move |_fingerprints,
                  hash_locks,
                  wallet_pk,
                  _wallet_signature,
                  substitute_clowder_id| {
                Ok(issue(
                    beta_proofs,
                    &hash_locks,
                    wallet_pk,
                    substitute_clowder_id,
                ))
            },
        );

    wlt.read()
        .await
        .offline_exchange(
            &substitute,
            alpha_proofs,
            test_pub_key(),
            test_pub_key(),
            [7u8; 32],
        )
        .await
}

#[tokio::test]
async fn test_offline_exchange_retry_sends_the_same_request() {
    let mut ctx = wallet_ctx();
    ctx.debit
        .expect_exchange_key()
        .returning(|proofs| crate::pocket::exchange_key(&zero_seed(), proofs));
    let wlt = wallet(ctx).await;

    let (_alpha_info, alpha_keyset) = core_tests::generate_random_ecash_keyset();
    let mut alpha_proofs =
        core_tests::generate_random_ecash_proofs(&alpha_keyset, &[Amount::from(8)]);
    add_test_dleqs(&mut alpha_proofs);

    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let mut substitute = MockClowderMintConnector::new();
    substitute
        .expect_post_offline_exchange()
        .times(2)
        .returning(
            move |_fingerprints,
                  _hash_locks,
                  wallet_pk,
                  wallet_signature,
                  _substitute_clowder_id| {
                seen.lock().unwrap().push((wallet_pk, wallet_signature));
                Ok(vec![])
            },
        );

    for _ in 0..2 {
        let _ = wlt
            .read()
            .await
            .offline_exchange(
                &substitute,
                alpha_proofs.clone(),
                test_pub_key(),
                test_pub_key(),
                [7u8; 32],
            )
            .await;
    }

    let requests = requests.lock().unwrap();
    assert_eq!(requests[0], requests[1]);
}

#[tokio::test]
async fn test_offline_exchange_success_with_valid_proofs() {
    let res = offline_exchange_issuing(exchange_locked).await.unwrap();

    assert_eq!(res.len(), 1);
    assert_eq!(res[0].amount, Amount::from(8));

    assert!(res[0].witness.is_some());
}

#[tokio::test]
async fn test_offline_exchange_rejects_proofs_another_key_can_refund() {
    let res = offline_exchange_issuing(|proofs, hash_locks, wallet_pk, _issuer| {
        exchange_locked(proofs, hash_locks, wallet_pk, test_other_pub_key())
    })
    .await;
    assert!(matches!(res, Err(Error::Swap(_))));
}

#[tokio::test]
async fn test_offline_exchange_rejects_proofs_without_the_offline_tag() {
    let res = offline_exchange_issuing(|mut proofs, hash_locks, wallet_pk, issuer| {
        let locktime = time::OffsetDateTime::now_utc().unix_timestamp() as u64 + 3600;
        for (proof, hash_lock) in proofs.iter_mut().zip(hash_locks) {
            let htlc =
                htlc::exchange_htlc(*hash_lock, locktime, wallet_pk.into(), issuer.into()).unwrap();
            proof.secret = htlc.try_into().unwrap();
        }
        proofs
    })
    .await;
    assert!(matches!(res, Err(Error::Swap(_))));
}

#[tokio::test]
async fn test_create_shareable_remote_payment_request_can_be_prepared() {
    let mut ctx = wallet_ctx();
    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.debit
        .expect_prepare_send()
        .times(1)
        .returning(|amount, _infos| {
            assert_eq!(amount, Amount::from(42));
            Ok(Default::default())
        });

    ctx.payment_request_repo
        .expect_add_payment_request()
        .times(1)
        .returning(|_| Ok(()));

    let wlt = wallet(ctx).await;

    let (_, shared_request) = wlt
        .read()
        .await
        .create_shareable_remote_payment_request(
            Amount::from(42),
            CurrencyUnit::Sat,
            Some("shared request memo".to_string()),
        )
        .await
        .unwrap();

    assert!(!shared_request.is_empty());

    let summary = wlt
        .read()
        .await
        .prepare_pay_shared_payment_request(shared_request)
        .await
        .unwrap();

    assert_eq!(summary.ptype, PaymentType::PaymentRequest);
}

#[tokio::test]
async fn test_prepare_pay_shared_payment_request_sets_payment_reference() {
    let mut ctx = wallet_ctx();
    let expected_node_id = NodeId::new(test_pub_key(), bitcoin::Network::Testnet);

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.client
        .expect_mint_url()
        .times(1)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.debit
        .expect_prepare_send()
        .times(1)
        .returning(|amount, _infos| {
            assert_eq!(amount, Amount::from(42));
            Ok(Default::default())
        });

    ctx.payment_request_repo
        .expect_add_payment_request()
        .times(1)
        .returning(|_| Ok(()));

    let wlt = wallet(ctx).await;

    let (shared_request_id, shared_request) = wlt
        .read()
        .await
        .create_shareable_remote_payment_request(
            Amount::from(42),
            CurrencyUnit::Sat,
            Some("shared request memo".to_string()),
        )
        .await
        .unwrap();

    let summary = wlt
        .read()
        .await
        .prepare_pay_shared_payment_request(shared_request)
        .await
        .unwrap();

    assert_eq!(summary.ptype, PaymentType::PaymentRequest);

    let wallet_guard = wlt.read().await;
    let payment_guard = wallet_guard.current_payment.lock().await;
    let payment = payment_guard.as_ref().expect("payment reference is set");

    assert_eq!(payment.unit, CurrencyUnit::Sat);
    assert_eq!(payment.memo, Some("shared request memo".to_string()));

    match &payment.ptype {
        WalletPaymentType::PaymentRequest {
            node_id,
            payment_request_id,
        } => {
            assert_eq!(node_id, &expected_node_id);
            assert_eq!(payment_request_id, &Some(shared_request_id));
        }
        other => panic!("unexpected payment type: {other:?}"),
    }
}

#[tokio::test]
async fn test_pay_shared_payment_request() {
    let mut ctx = wallet_ctx();

    let pid = Uuid::new_v4();
    let req_id = Uuid::new_v4();
    let tx_id = Uuid::new_v4();
    let receiver_node_id = node_id(NODE_ID_1);
    let expected_receiver_node_id = receiver_node_id.clone();
    let relays = vec![RelayUrl::from_str("wss://relay.example.com").unwrap()];

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client
        .expect_mint_url()
        .times(2)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.nostr_transport
        .expect_relays()
        .return_const(vec![RelayUrl::from_str("wss://test.example.com").unwrap()]);

    ctx.contact_repo
        .expect_get_contacts_by_node_id()
        .times(1)
        .returning(|_| Ok(vec![]));

    let relays_clone = relays.clone();
    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .returning(move |_, _| Ok(relays_clone.clone()));

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit
        .expect_send_proofs()
        .times(1)
        .returning(|_rid, _infos, _client, _swap| Ok(Default::default()));

    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .returning(|_, _| Ok(EventId::from_byte_array([0u8; 32])));

    ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
        assert_eq!(tx.direction, TransactionDirection::Outgoing);
        assert_eq!(tx.amount, Amount::ZERO);
        assert_eq!(tx.fees.swap, Amount::ZERO);
        assert_eq!(tx.fees.melt, Amount::ZERO);
        assert_eq!(tx.fees.network, Amount::ZERO);
        assert_eq!(tx.unit, CurrencyUnit::Sat);
        assert_eq!(tx.tstamp, 123);
        assert_eq!(tx.memo, Some("shared request memo".to_string()));
        assert_eq!(tx.payment_type, PaymentType::PaymentRequest);
        assert_eq!(tx.status, TransactionStatus::Pending);
        assert_eq!(tx.contact_node_id, Some(expected_receiver_node_id.clone()));
        assert_eq!(tx.payment_request_id, Some(req_id));
        assert_eq!(tx.nostr_event_id, Some(EventId::from_byte_array([0u8; 32])));
        Ok(tx_id)
    });

    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(|id, _| {
            Err(bcr_wallet_persistence::error::Error::PaymentRequestNotFound(id.to_string()))
        });

    let wlt = wallet(ctx).await;

    *wlt.read().await.current_payment.lock().await = Some(PayReference {
        request_id: pid,
        unit: CurrencyUnit::Sat,
        fees: TransactionFees::default(),
        ptype: WalletPaymentType::PaymentRequest {
            node_id: receiver_node_id,
            payment_request_id: Some(req_id),
        },
        memo: Some("shared request memo".to_string()),
    });
    let (res_tx_id, token) = wlt.read().await.pay(pid, 123).await.unwrap();

    assert_eq!(res_tx_id, tx_id);
    assert!(token.is_none());
}

#[tokio::test]
async fn test_pay_payment_request_marks_local_request_paid() {
    let mut ctx = wallet_ctx();

    let pid = Uuid::new_v4();
    let req_id = Uuid::new_v4();
    let tx_id = Uuid::new_v4();
    let receiver_node_id = node_id(NODE_ID_1);
    let expected_receiver_node_id = receiver_node_id.clone();
    let relays = vec![RelayUrl::from_str("wss://relay.example.com").unwrap()];

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client
        .expect_mint_url()
        .times(2)
        .return_const(url::Url::from_str("https://mint.example").unwrap());

    ctx.nostr_transport
        .expect_relays()
        .return_const(vec![RelayUrl::from_str("wss://test.example.com").unwrap()]);

    ctx.contact_repo
        .expect_get_contacts_by_node_id()
        .times(1)
        .returning(|_| Ok(vec![]));

    let relays_clone = relays.clone();
    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .returning(move |_, _| Ok(relays_clone.clone()));

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);

    ctx.debit
        .expect_send_proofs()
        .times(1)
        .returning(|_rid, _infos, _client, _swap| Ok(Default::default()));

    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .returning(|_, _| Ok(EventId::from_byte_array([0u8; 32])));

    ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
        assert_eq!(tx.direction, TransactionDirection::Outgoing);
        assert_eq!(tx.amount, Amount::ZERO);
        assert_eq!(tx.fees.swap, Amount::ZERO);
        assert_eq!(tx.fees.melt, Amount::ZERO);
        assert_eq!(tx.fees.network, Amount::ZERO);
        assert_eq!(tx.unit, CurrencyUnit::Sat);
        assert_eq!(tx.tstamp, 123);
        assert_eq!(tx.memo, Some("shared request memo".to_string()));
        assert_eq!(tx.payment_type, PaymentType::PaymentRequest);
        assert_eq!(tx.status, TransactionStatus::Pending);
        assert_eq!(tx.contact_node_id, Some(expected_receiver_node_id.clone()));
        assert_eq!(tx.payment_request_id, Some(req_id));
        assert_eq!(tx.nostr_event_id, Some(EventId::from_byte_array([0u8; 32])));
        Ok(tx_id)
    });

    ctx.payment_request_repo
        .expect_apply_payment_request_transition()
        .times(1)
        .returning(move |id, transition| {
            assert_eq!(id, req_id);
            assert_eq!(transition.target_state, PaymentRequestState::Paid { tx_id });
            Ok(PaymentRequestTransitionOutcome::Applied)
        });

    let wlt = wallet(ctx).await;

    *wlt.read().await.current_payment.lock().await = Some(PayReference {
        request_id: pid,
        unit: CurrencyUnit::Sat,
        fees: TransactionFees::default(),
        ptype: WalletPaymentType::PaymentRequest {
            node_id: receiver_node_id,
            payment_request_id: Some(req_id),
        },
        memo: Some("shared request memo".to_string()),
    });
    let (res_tx_id, token) = wlt.read().await.pay(pid, 123).await.unwrap();

    assert_eq!(res_tx_id, tx_id);
    assert!(token.is_none());
}

#[tokio::test]
async fn test_prepare_pay_to_node_id() {
    let mut ctx = wallet_ctx();
    let receiver = node_id(NODE_ID_1);

    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);
    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(|| Ok(vec![]));
    ctx.debit
        .expect_prepare_send()
        .times(1)
        .returning(|amount, _infos| {
            assert_eq!(amount, Amount::from(21));
            Ok(Default::default())
        });

    let wlt = wallet(ctx).await;

    let summary = wlt
        .read()
        .await
        .prepare_pay_to_node_id(
            receiver.clone(),
            Amount::from(21),
            CurrencyUnit::Sat,
            Some("node memo".to_string()),
        )
        .await
        .unwrap();

    assert_eq!(summary.ptype, PaymentType::Contact);

    let wallet_guard = wlt.read().await;
    let payment_guard = wallet_guard.current_payment.lock().await;
    let payment = payment_guard.as_ref().expect("payment reference is set");
    assert_eq!(payment.memo, Some("node memo".to_string()));
    match &payment.ptype {
        WalletPaymentType::PaymentRequest {
            node_id,
            payment_request_id,
        } => {
            assert_eq!(node_id, &receiver);
            assert!(payment_request_id.is_none());
        }
        other => panic!("unexpected payment type: {other:?}"),
    }
}

#[tokio::test]
async fn test_prepare_pay_to_node_id_wrong_network() {
    let mut ctx = wallet_ctx();
    ctx.debit
        .expect_unit()
        .times(1)
        .returning(|| CurrencyUnit::Sat);
    let wlt = wallet(ctx).await;

    let res = wlt
        .read()
        .await
        .prepare_pay_to_node_id(
            NodeId::new(test_other_pub_key(), bitcoin::Network::Bitcoin),
            Amount::from(21),
            CurrencyUnit::Sat,
            None,
        )
        .await;

    assert!(matches!(res, Err(Error::InvalidNetwork(..))));
    assert!(wlt.read().await.current_payment.lock().await.is_none());
}

#[tokio::test]
async fn test_pay_to_node_id() {
    let mut ctx = wallet_ctx();
    let tx_id = Uuid::new_v4();
    let receiver = node_id(NODE_ID_1);
    let expected_receiver = receiver.clone();
    let expected_npub = receiver.npub();
    let known_relay = RelayUrl::from_str("wss://test.example.com").unwrap();

    ctx.debit
        .expect_unit()
        .times(2)
        .returning(|| CurrencyUnit::Sat);
    ctx.client
        .expect_get_mint_keysets()
        .times(2)
        .returning(|| Ok(vec![]));
    ctx.client
        .expect_mint_url()
        .times(2)
        .return_const(url::Url::from_str("https://mint.example").unwrap());
    ctx.debit
        .expect_prepare_send()
        .times(1)
        .returning(|_amount, _infos| Ok(Default::default()));
    ctx.debit
        .expect_send_proofs()
        .times(1)
        .returning(|_rid, _infos, _client, _swap| Ok(Default::default()));
    ctx.contact_repo
        .expect_get_contacts_by_node_id()
        .times(1)
        .returning(|_| Ok(vec![]));
    ctx.nostr_transport
        .expect_relays()
        .return_const(vec![known_relay.clone()]);
    ctx.nostr_transport
        .expect_fetch_relay_list()
        .times(1)
        .returning(|_, _| Err(TransportError::Network("offline".to_string())));
    ctx.nostr_transport
        .expect_send_private_msg()
        .times(1)
        .withf(move |recipient, _| {
            Nip19Profile::from_bech32(recipient).is_ok_and(|p| {
                p.public_key == expected_npub && p.relays == vec![known_relay.clone()]
            })
        })
        .returning(|_, _| Ok(EventId::from_byte_array([0u8; 32])));
    ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
        assert_eq!(tx.direction, TransactionDirection::Outgoing);
        assert_eq!(tx.payment_type, PaymentType::Contact);
        assert_eq!(tx.status, TransactionStatus::Pending);
        assert_eq!(tx.contact_node_id, Some(expected_receiver.clone()));
        assert!(tx.payment_request_id.is_none());
        assert_eq!(tx.nostr_event_id, Some(EventId::from_byte_array([0u8; 32])));
        Ok(tx_id)
    });

    let wlt = wallet(ctx).await;

    let summary = wlt
        .read()
        .await
        .prepare_pay_to_node_id(receiver, Amount::from(21), CurrencyUnit::Sat, None)
        .await
        .unwrap();

    let (res_tx_id, token) = wlt.read().await.pay(summary.request_id, 123).await.unwrap();

    assert_eq!(res_tx_id, tx_id);
    assert!(token.is_none());
}

#[tokio::test]
async fn test_reclaim_foreign_mint_proofs_success() {
    let mut ctx = wallet_ctx();

    let mint_url = url::Url::from_str("https://mint.example").unwrap();
    let clowder_id = test_pub_key();
    let tx_id = Uuid::new_v4();

    let (info, _keyset, proofs) = test_keyset_and_proofs(&[Amount::from(8), Amount::from(16)]);

    let keyset_infos = HashMap::from([(info.id, info.clone())]);

    let expected_ys: Vec<cashu::PublicKey> = proofs
        .iter()
        .map(|proof| proof.y().expect("valid proof y"))
        .collect();

    let foreign_mint_proofs = vec![
        ForeignMintProof {
            clowder_id,
            proof: proofs[0].clone(),
            reason: ForeignMintProofReason::MintOffline,
        },
        ForeignMintProof {
            clowder_id,
            proof: proofs[1].clone(),
            reason: ForeignMintProofReason::WalletOffline,
        },
    ];

    ctx.client
        .expect_get_clowder_betas()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client.expect_mint_url().return_const(mint_url.clone());

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(move || Ok(keyset_infos.values().cloned().collect()));

    let check_ys = expected_ys.clone();
    ctx.client
        .expect_post_check_state()
        .times(1)
        .withf(move |req| req.ys.iter().collect::<HashSet<_>>() == check_ys.iter().collect())
        .returning(|req| {
            Ok(req
                .ys
                .into_iter()
                .map(|y| cashu::ProofState {
                    y,
                    state: cashu::State::Unspent,
                    witness: None,
                })
                .collect())
        });

    ctx.debit
        .expect_fetch_foreign_mint_proofs()
        .times(1)
        .return_once(move || Ok(foreign_mint_proofs));

    ctx.debit.expect_unit().returning(|| CurrencyUnit::Sat);

    let received_ys = expected_ys.clone();

    ctx.debit.expect_receive_proofs().times(1).return_once(
        move |_client, received_keysets, received_proofs, _swap_config| {
            assert!(received_keysets.contains_key(&info.id));
            assert_eq!(received_proofs.len(), 2);
            assert_eq!(received_proofs.total_amount().unwrap(), Amount::from(24),);

            Ok((Amount::from(24), received_ys))
        },
    );

    let deleted_ys = expected_ys.clone();
    ctx.debit
        .expect_delete_foreign_mint_proofs()
        .times(1)
        .withf(move |actual_clowder_id, actual_ys| {
            *actual_clowder_id == clowder_id && *actual_ys == deleted_ys
        })
        .returning(|_, _| ());

    ctx.tx_repo
        .expect_store_tx()
        .times(1)
        .return_once(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Incoming,);
            assert_eq!(tx.amount, Amount::from(24));
            assert_eq!(tx.fees.swap, Amount::ZERO);
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.payment_type, PaymentType::Token);
            assert_eq!(tx.status, TransactionStatus::Settled,);

            Ok(tx_id)
        });

    let wlt = wallet(ctx).await;
    let reclaimed = wlt
        .read()
        .await
        .reclaim_foreign_mint_proofs()
        .await
        .expect("reclaim_foreign_mint_proofs works");
    assert_eq!(reclaimed, Amount::from(24));
}

#[tokio::test]
async fn reclaim_foreign_mint_proofs_drops_spent() {
    let mut ctx = wallet_ctx();

    let mint_url = url::Url::from_str("https://mint.example").unwrap();
    let clowder_id = test_pub_key();
    let tx_id = Uuid::new_v4();

    let (info, _keyset, proofs) =
        test_keyset_and_proofs(&[Amount::from(8), Amount::from(16), Amount::from(4)]);
    let keyset_infos = HashMap::from([(info.id, info.clone())]);

    let spent_y = proofs[0].y().expect("valid proof y");
    let unspent_ys: Vec<cashu::PublicKey> = vec![
        proofs[1].y().expect("valid proof y"),
        proofs[2].y().expect("valid proof y"),
    ];

    let foreign_mint_proofs = vec![
        ForeignMintProof {
            clowder_id,
            proof: proofs[0].clone(),
            reason: ForeignMintProofReason::MintOffline,
        },
        ForeignMintProof {
            clowder_id,
            proof: proofs[1].clone(),
            reason: ForeignMintProofReason::MintOffline,
        },
        ForeignMintProof {
            clowder_id,
            proof: proofs[2].clone(),
            reason: ForeignMintProofReason::WalletOffline,
        },
    ];

    ctx.client
        .expect_get_clowder_betas()
        .times(1)
        .returning(|| Ok(vec![]));

    ctx.client.expect_mint_url().return_const(mint_url.clone());

    ctx.client
        .expect_get_mint_keysets()
        .times(1)
        .returning(move || Ok(keyset_infos.values().cloned().collect()));

    ctx.client
        .expect_post_check_state()
        .times(1)
        .returning(move |req| {
            Ok(req
                .ys
                .into_iter()
                .map(|y| cashu::ProofState {
                    y,
                    state: if y == spent_y {
                        cashu::State::Spent
                    } else {
                        cashu::State::Unspent
                    },
                    witness: None,
                })
                .collect())
        });

    ctx.debit
        .expect_fetch_foreign_mint_proofs()
        .times(1)
        .return_once(move || Ok(foreign_mint_proofs));

    ctx.debit.expect_unit().returning(|| CurrencyUnit::Sat);

    let received_ys = unspent_ys.clone();
    ctx.debit.expect_receive_proofs().times(1).return_once(
        move |_client, received_keysets, received_proofs, _swap_config| {
            assert!(received_keysets.contains_key(&info.id));
            assert_eq!(received_proofs.len(), 2);
            assert_eq!(received_proofs.total_amount().unwrap(), Amount::from(20));

            Ok((Amount::from(20), received_ys))
        },
    );

    ctx.debit
        .expect_delete_foreign_mint_proofs()
        .times(1)
        .withf(move |actual_clowder_id, actual_ys| {
            *actual_clowder_id == clowder_id && *actual_ys == vec![spent_y]
        })
        .returning(|_, _| ());

    let deleted_unspent_ys = unspent_ys.clone();
    ctx.debit
        .expect_delete_foreign_mint_proofs()
        .times(1)
        .withf(move |actual_clowder_id, actual_ys| {
            *actual_clowder_id == clowder_id && *actual_ys == deleted_unspent_ys
        })
        .returning(|_, _| ());

    ctx.tx_repo
        .expect_store_tx()
        .times(1)
        .return_once(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Incoming);
            assert_eq!(tx.amount, Amount::from(20));
            assert_eq!(tx.status, TransactionStatus::Settled);
            Ok(tx_id)
        });

    let wlt = wallet(ctx).await;
    let reclaimed = wlt
        .read()
        .await
        .reclaim_foreign_mint_proofs()
        .await
        .expect("reclaim_foreign_mint_proofs works");
    assert_eq!(reclaimed, Amount::from(20));
}

async fn reclaim_rejecting(rejected: &'static [u64]) -> (Amount, Vec<Vec<cashu::PublicKey>>) {
    let mut ctx = wallet_ctx();
    let mint_url = url::Url::from_str("https://mint.example").unwrap();
    let clowder_id = test_pub_key();
    let (info, _keyset, proofs) = test_keyset_and_proofs(&[Amount::from(8), Amount::from(16)]);
    let keyset_infos = HashMap::from([(info.id, info.clone())]);
    let foreign_mint_proofs = proofs
        .into_iter()
        .map(|proof| ForeignMintProof {
            clowder_id,
            proof,
            reason: ForeignMintProofReason::MintOffline,
        })
        .collect();

    ctx.client
        .expect_get_clowder_betas()
        .returning(|| Ok(vec![]));
    ctx.client.expect_mint_url().return_const(mint_url);
    ctx.client
        .expect_get_mint_keysets()
        .returning(move || Ok(keyset_infos.values().cloned().collect()));
    ctx.client.expect_post_check_state().returning(|req| {
        Ok(req
            .ys
            .into_iter()
            .map(|y| cashu::ProofState {
                y,
                state: cashu::State::Unspent,
                witness: None,
            })
            .collect())
    });
    ctx.debit
        .expect_fetch_foreign_mint_proofs()
        .return_once(move || Ok(foreign_mint_proofs));
    ctx.debit.expect_unit().returning(|| CurrencyUnit::Sat);
    ctx.debit
        .expect_receive_proofs()
        .returning(move |_client, _keysets, proofs, _swap_config| {
            if proofs.len() > 1 || rejected.contains(&proofs[0].amount.to_u64()) {
                return Err(Error::Mint(
                    bcr_common::client::mint::Error::InvalidRequest(
                        bcr_common::client::mint::BRError::Generic("proof rejected".into()),
                    ),
                ));
            }
            Ok((proofs[0].amount, vec![proofs[0].y().unwrap()]))
        });
    let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = deleted.clone();
    ctx.debit
        .expect_delete_foreign_mint_proofs()
        .returning(move |_, ys| recorded.lock().unwrap().push(ys));
    ctx.tx_repo
        .expect_store_tx()
        .returning(|_| Ok(Uuid::new_v4()));

    let wlt = wallet(ctx).await;
    let reclaimed = wlt
        .read()
        .await
        .reclaim_foreign_mint_proofs()
        .await
        .expect("reclaim_foreign_mint_proofs works");
    let deleted = deleted.lock().unwrap().clone();
    (reclaimed, deleted)
}

#[tokio::test]
async fn reclaim_foreign_mint_proofs_drops_a_proof_the_mint_rejects() {
    let (reclaimed, deleted) = reclaim_rejecting(&[16]).await;
    assert_eq!(reclaimed, Amount::from(8));
    assert_eq!(deleted.len(), 2, "the reclaimed and the rejected proof");
}

#[tokio::test]
async fn reclaim_foreign_mint_proofs_keeps_proofs_when_the_mint_rejects_all() {
    let (reclaimed, deleted) = reclaim_rejecting(&[8, 16]).await;
    assert_eq!(reclaimed, Amount::ZERO);
    assert!(
        deleted.is_empty(),
        "a mint rejecting everything proves no proof broken"
    );
}

#[tokio::test]
async fn test_offline_pay_by_token() {
    let mut ctx = wallet_ctx();

    let request_id = Uuid::new_v4();
    let tx_id = Uuid::new_v4();
    let now = 123;
    let send_amount = Amount::from(16u64);
    let fees = TransactionFees {
        swap: Amount::from(1u64),
        ..Default::default()
    };
    let memo = Some("offline token payment".to_string());
    let wallet_mint_url = url::Url::from_str("https://wallet-mint.example").unwrap();
    let substitute_url = url::Url::from_str("https://substitute.example").unwrap();
    let substitute_beta_url = url::Url::from_str("https://substitute-beta.example").unwrap();

    let substitute_keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
    let substitute_clowder_id = secp256k1::PublicKey::from_keypair(&substitute_keypair);

    // 24 total, 16 payment, 8 change
    let (_local_info, _local_keyset, mut local_proofs) =
        test_keyset_and_proofs(&[Amount::from(8u64), Amount::from(16u64)]);

    add_test_dleqs(&mut local_proofs);
    let local_proofs_by_y: HashMap<_, _> = local_proofs
        .iter()
        .cloned()
        .map(|proof| (proof.y().unwrap(), proof))
        .collect();

    let (substitute_info, substitute_mint_keyset, mut substitute_proofs) =
        test_keyset_and_proofs(&[Amount::from(8u64), Amount::from(16u64)]);
    let substitute_kid = substitute_info.id;
    add_test_dleqs(&mut substitute_proofs);
    let unlocked_payment_proofs =
        core_tests::generate_random_ecash_proofs(&substitute_mint_keyset, &[send_amount]);
    assert!(
        unlocked_payment_proofs
            .iter()
            .all(|proof| { proof.witness.is_none() && proof.p2pk_e.is_none() })
    );

    let expected_payment_ys: Vec<_> = unlocked_payment_proofs
        .iter()
        .map(|proof| proof.y().unwrap())
        .collect();

    ctx.client
        .expect_mint_url()
        .return_const(wallet_mint_url.clone());

    ctx.debit.expect_unit().returning(|| CurrencyUnit::Sat);
    ctx.debit
        .expect_exchange_key()
        .returning(|proofs| crate::pocket::exchange_key(&zero_seed(), proofs));

    let local_proofs_for_mock = local_proofs_by_y.clone();

    ctx.debit
        .expect_return_proofs_to_send_for_offline_payment()
        .times(1)
        .return_once(move |actual_request_id| {
            assert_eq!(actual_request_id, request_id);

            Ok((send_amount, local_proofs_for_mock, Default::default()))
        });

    let unlocked_payment_proofs_for_mock = unlocked_payment_proofs.clone();
    ctx.debit
        .expect_swap_to_unlocked_substitute_proofs()
        .times(1)
        .return_once(
            move |received_substitute_proofs,
                  received_keyset_infos,
                  received_keysets,
                  _substitute_client,
                  received_clowder_id,
                  _beta_provider,
                  received_send_amount,
                  received_swap_config| {
                assert_eq!(
                    received_substitute_proofs.total_amount().unwrap(),
                    Amount::from(24u64)
                );
                assert!(
                    received_substitute_proofs
                        .iter()
                        .all(|proof| proof.witness.is_some())
                );
                assert_eq!(received_clowder_id, substitute_clowder_id);
                assert_eq!(received_send_amount, send_amount);
                assert_eq!(received_swap_config.alpha_pk, substitute_clowder_id);
                assert_eq!(received_swap_config.expiry, time::Duration::seconds(60));
                assert!(received_keyset_infos.contains_key(&substitute_kid));
                assert!(received_keysets.contains_key(&substitute_kid));
                Ok(unlocked_payment_proofs_for_mock)
            },
        );

    let expected_ys_for_tx = expected_payment_ys.clone();
    let expected_memo_for_tx = memo.clone();
    let expected_substitute_url_for_tx = substitute_url.clone();

    ctx.tx_repo
        .expect_store_tx()
        .times(1)
        .return_once(move |tx| {
            assert_eq!(tx.mint_url, to_mint_url(&expected_substitute_url_for_tx));
            assert_eq!(tx.direction, TransactionDirection::Outgoing);
            assert_eq!(tx.amount, send_amount);
            assert_eq!(tx.fees.swap, Amount::from(1u64));
            assert_eq!(tx.fees.melt, Amount::ZERO);
            assert_eq!(tx.fees.network, Amount::ZERO);
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.tstamp, now);
            assert_eq!(tx.memo, expected_memo_for_tx);
            assert_eq!(tx.payment_type, PaymentType::Token);
            assert_eq!(tx.status, TransactionStatus::Pending);
            assert_eq!(tx.ys, expected_ys_for_tx);
            assert!(tx.quote_id.is_none());
            assert!(tx.payment_request_id.is_none());
            assert!(tx.nostr_event_id.is_none());
            assert!(tx.btc_tx_id.is_none());
            assert!(tx.contact_node_id.is_none());

            Ok(tx_id)
        });

    let mut substitute_client = MockClowderMintConnector::new();
    let voted_substitute_url = substitute_url.clone();
    substitute_client
        .expect_get_alpha_substitute()
        .times(1)
        .return_once(move |_wallet_clowder_id| {
            Ok(wire_clowder::ConnectedMintResponse {
                mint: voted_substitute_url,
                clowder: url::Url::from_str("https://substitute-clowder.example").unwrap(),
                node_id: substitute_clowder_id,
            })
        });
    substitute_client
        .expect_get_clowder_id()
        .times(1)
        .returning(move || Ok(substitute_clowder_id));
    substitute_client
        .expect_get_alpha_offline()
        .times(1)
        .returning(|_| {
            Ok(wire_clowder::OfflineResponse {
                offline: true,
                evidence_digest: Some([7u8; 32]),
            })
        });

    let beta_url_for_response = substitute_beta_url.clone();
    substitute_client
        .expect_get_clowder_betas()
        .times(1)
        .return_once(move || {
            Ok(vec![ClowderBeta {
                url: beta_url_for_response,
                clowder_id: test_pub_key(),
            }])
        });
    let exchanged_proofs_for_mock = substitute_proofs.clone();
    substitute_client
        .expect_post_offline_exchange()
        .times(1)
        .return_once(
            move |fingerprints,
                  hash_locks,
                  wallet_public_key,
                  _wallet_signature,
                  received_substitute_clowder_id| {
                assert_eq!(fingerprints.len(), 2);
                assert_eq!(hash_locks.len(), 2);
                assert_eq!(received_substitute_clowder_id, substitute_clowder_id);

                Ok(exchange_locked(
                    exchanged_proofs_for_mock,
                    &hash_locks,
                    wallet_public_key,
                    received_substitute_clowder_id,
                ))
            },
        );
    let substitute_info_for_mock = substitute_info.clone();
    substitute_client
        .expect_get_mint_keysets()
        .times(1)
        .return_once(move || Ok(vec![substitute_info_for_mock]));

    let substitute_keyset_for_mock = substitute_mint_keyset.clone();
    substitute_client
        .expect_get_mint_keyset()
        .times(1)
        .return_once(move |received_kid| {
            assert_eq!(received_kid, substitute_kid);

            Ok(bcr_wallet_core::util::to_keyset(
                &substitute_keyset_for_mock,
                None,
            ))
        });

    let substitute_client: Arc<dyn ClowderMintConnector> = Arc::new(substitute_client);
    let substitute_beta: Arc<dyn ClowderMintConnector> = Arc::new(MockClowderMintConnector::new());

    let mut wlt = wallet(ctx).await;
    wlt = wallet_with_betas(wlt, vec![(substitute_url.clone(), substitute_client)]).await;
    let expected_factory_url = substitute_beta_url.clone();
    wlt.write().await.client_factory = Box::new(move |actual_url| {
        assert_eq!(actual_url, expected_factory_url);
        substitute_beta.clone()
    });

    let (result_tx_id, token) = wlt
        .read()
        .await
        .offline_pay_by_token(request_id, CurrencyUnit::Sat, fees, memo.clone(), now)
        .await
        .expect("offline token payment works");
    assert_eq!(result_tx_id, tx_id);
    let token = token.expect("offline token payment returns a token");
    assert_eq!(from_mint_url(&token.mint_url().unwrap()), substitute_url);
    assert_eq!(token.unit(), Some(CurrencyUnit::Sat));
    assert_eq!(token.memo().as_deref(), memo.as_deref());
    let token_proofs = token
        .proofs(&[substitute_info])
        .expect("returned token contains valid substitute proofs");
    assert_eq!(token_proofs.total_amount().unwrap(), send_amount);
    assert_eq!(token_proofs.len(), 1);
}

struct ExchangeLockFixture {
    pdb: Arc<bcr_wallet_persistence::redb::pocket::PocketDB>,
    alpha_client: MockClowderMintConnector,
    alpha_proofs: Vec<cashu::Proof>,
    alpha_url: url::Url,
    path: Vec<wire_clowder::ConnectedMintResponse>,
    keyset: ecash::MintKeySet,
}

fn exchange_lock_fixture(alpha_signs: bool) -> ExchangeLockFixture {
    let pdb = Arc::new(
        bcr_wallet_persistence::test_utils::tests::in_memory_pocket_db("w-1", CurrencyUnit::Sat),
    );

    let alpha_url = url::Url::from_str("https://alpha.test").unwrap();
    let alpha_beta_url = url::Url::from_str("https://alpha-beta.test").unwrap();
    let (info, keyset) = core_tests::generate_random_ecash_keyset();
    let kid = info.id;
    let k_infos = test_kinfos(info);
    let alpha_proofs =
        core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8), Amount::from(2)]);

    let mut alpha_client = MockClowderMintConnector::new();
    alpha_client.expect_get_clowder_betas().returning(move || {
        Ok(vec![ClowderBeta {
            url: alpha_beta_url.clone(),
            clowder_id: test_pub_key(),
        }])
    });
    alpha_client
        .expect_get_mint_keysets()
        .returning(move || Ok(k_infos.values().cloned().collect()));
    let keyset_for_lookup = keyset.clone();
    alpha_client
        .expect_get_mint_keyset()
        .with(mockall::predicate::eq(kid))
        .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_for_lookup, None)));
    let keyset_for_signing = keyset.clone();

    alpha_client
        .expect_post_swap_commitment()
        .times(1)
        .returning(|inputs, outputs, _, _, _| {
            Ok(crate::external::mint::SwapCommitmentResult {
                inputs_ys: inputs.iter().map(|p| p.y().unwrap()).collect(),
                outputs,
                ..mock_commitment_result()
            })
        });
    alpha_client
        .expect_post_swap_committed()
        .times(1)
        .returning(move |_, outputs, _| {
            if !alpha_signs {
                return Err(Error::Swap("connection reset".into()));
            }
            let amounts = outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
            Ok(core_tests::generate_ecash_signatures(
                &keyset_for_signing,
                &amounts,
            ))
        });

    let hop = |mint: &str, node_id| wire_clowder::ConnectedMintResponse {
        mint: url::Url::from_str(mint).unwrap(),
        clowder: url::Url::from_str("https://clowder.test").unwrap(),
        node_id,
    };
    let path = vec![
        hop("https://alpha.test", test_other_pub_key()),
        hop("https://middle.test", test_pub_key()),
        hop("https://wallet-mint.test", test_pub_key()),
    ];

    ExchangeLockFixture {
        pdb,
        alpha_client,
        alpha_proofs,
        alpha_url,
        path,
        keyset,
    }
}

async fn exchange_lock_wallet(
    mut ctx: MockWalletCtx,
    pdb: Arc<bcr_wallet_persistence::redb::pocket::PocketDB>,
) -> Arc<RwLock<Wallet>> {
    ctx.client
        .expect_mint_url()
        .return_const(url::Url::from_str("https://wallet-mint.test").unwrap());
    let debit = crate::pocket::debit::Pocket::new(
        CurrencyUnit::Sat,
        pdb,
        Arc::new(bcr_wallet_persistence::MockMintMeltRepository::new()),
        bip39::Mnemonic::generate(bip39::WordCount::Words12)
            .unwrap()
            .to_seed(""),
        Arc::new(crate::pocket::test_utils::tests::test_beta_provider()),
    );
    let wlt = wallet_with_debit(ctx, Box::new(debit)).await;
    let mut alpha_beta = MockClowderMintConnector::new();
    setup_attestation_mock(&mut alpha_beta);
    let alpha_beta: Arc<dyn ClowderMintConnector> = Arc::new(alpha_beta);
    wlt.write().await.client_factory = Box::new(move |_| alpha_beta.clone());
    wlt
}

fn secrets(proofs: &[cashu::Proof]) -> Vec<cashu::secret::Secret> {
    proofs.iter().map(|p| p.secret.clone()).collect()
}

#[tokio::test]
async fn exchange_record_before_lock_holds_locked_proofs_when_exchange_fails() {
    let fx = exchange_lock_fixture(true);
    let mut ctx = wallet_ctx();
    ctx.client
        .expect_post_online_exchange()
        .times(crate::config::MAX_INTERMINT_ATTEMPTS as usize)
        .returning(|_, _| {
            Err(bcr_common::client::mint::Error::Internal(
                "beta unreachable".to_string(),
            ))
        });
    let wlt = exchange_lock_wallet(ctx, fx.pdb.clone()).await;

    let tstamp = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
    let res = wlt
        .read()
        .await
        .online_exchange(
            fx.alpha_proofs.clone(),
            fx.alpha_url.clone(),
            Arc::new(fx.alpha_client),
            fx.path.clone(),
            tstamp,
        )
        .await;
    assert!(matches!(res, Err(Error::MaxExchangeAttempts)));

    let records = fx.pdb.list_exchange_records().await.unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    let locked = record.locked_proofs.as_ref().expect("locked proofs stored");
    assert_eq!(locked.total_amount().unwrap(), Amount::from(10));
    for proof in locked {
        assert_eq!(htlc::htlc_lock(proof).unwrap().0, record.hash_lock);
    }
    assert!(fx.pdb.list_commitments().await.unwrap().is_empty());
}

struct UnansweredLockSwap {
    fx: ExchangeLockFixture,
    wlt: Arc<RwLock<Wallet>>,
    record: bcr_wallet_persistence::ExchangeRecord,
    commitment: bcr_wallet_persistence::SwapCommitmentRecord,
}

async fn unanswered_lock_swap() -> UnansweredLockSwap {
    let mut fx = exchange_lock_fixture(false);
    let wlt = exchange_lock_wallet(wallet_ctx(), fx.pdb.clone()).await;
    let alpha_client = std::mem::replace(&mut fx.alpha_client, MockClowderMintConnector::new());
    let tstamp = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
    let res = wlt
        .read()
        .await
        .online_exchange(
            fx.alpha_proofs.clone(),
            fx.alpha_url.clone(),
            Arc::new(alpha_client),
            fx.path.clone(),
            tstamp,
        )
        .await;
    assert!(res.is_err());
    let mut records = fx.pdb.list_exchange_records().await.unwrap();
    assert_eq!(records.len(), 1);
    let record = records.pop().unwrap();
    assert!(record.locked_proofs.is_none());
    assert_eq!(secrets(&record.alpha_inputs), secrets(&fx.alpha_proofs));
    let mut commitments = fx.pdb.list_commitments().await.unwrap();
    assert_eq!(commitments.len(), 1);
    let commitment = commitments.pop().unwrap();
    UnansweredLockSwap {
        fx,
        wlt,
        record,
        commitment,
    }
}

fn sign_blinds(
    keyset: &bcr_common::ecash::MintKeySet,
    blinds: &[cashu::BlindedMessage],
) -> Vec<cashu::BlindSignature> {
    let amounts: Vec<Amount> = blinds.iter().map(|b| b.amount).collect();
    core_tests::generate_ecash_signatures(keyset, &amounts)
}

fn recovery_alpha(
    s: &UnansweredLockSwap,
    restored: bool,
    spend_state: Option<cashu::State>,
) -> MockClowderMintConnector {
    let mut alpha = MockClowderMintConnector::new();
    let blinds = s.record.blinded_messages();
    let signatures = sign_blinds(&s.fx.keyset, &blinds);
    let expected = blinds.clone();
    alpha
        .expect_post_restore()
        .times(1)
        .withf(move |req| req.outputs == expected)
        .returning(move |_| {
            Ok(if restored {
                blinds.iter().cloned().zip(signatures.clone()).collect()
            } else {
                vec![]
            })
        });
    match spend_state {
        None => {
            alpha.expect_post_check_state().times(0);
        }
        Some(state) => {
            let ys = s.commitment.inputs.clone();
            alpha
                .expect_post_check_state()
                .times(1)
                .withf(move |req| req.ys == ys)
                .returning(move |req| {
                    Ok(req
                        .ys
                        .iter()
                        .map(|y| cashu::ProofState {
                            y: *y,
                            state,
                            witness: None,
                        })
                        .collect())
                });
        }
    }
    alpha.expect_get_clowder_betas().returning(|| {
        Ok(vec![ClowderBeta {
            url: url::Url::from_str("https://alpha-beta.test").unwrap(),
            clowder_id: test_pub_key(),
        }])
    });
    let keyset = s.fx.keyset.clone();
    alpha
        .expect_get_mint_keyset()
        .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset, None)));
    alpha
}

fn recovery_beta(
    s: &UnansweredLockSwap,
    protest: Option<wire_common::ProtestStatus>,
) -> MockClowderMintConnector {
    let mut beta = MockClowderMintConnector::new();
    let Some(status) = protest else {
        beta.expect_post_protest_swap().times(0);
        return beta;
    };
    let signatures = sign_blinds(&s.fx.keyset, &s.record.blinded_messages());
    let alpha_id = s.record.alpha_clowder_id;
    let input_secrets = secrets(&s.record.alpha_inputs);
    let commitment = s.commitment.clone();
    beta.expect_post_protest_swap()
        .times(1)
        .withf(move |req| {
            let content = STANDARD.decode(&commitment.body_content).unwrap();
            let msg = secp256k1::Message::from_digest(Sha256::hash(&content).to_byte_array());
            let ephemeral = secp256k1::Keypair::from_secret_key(
                secp256k1::SECP256K1,
                &commitment.ephemeral_secret,
            );
            req.alpha_id == alpha_id
                && req
                    .proofs
                    .iter()
                    .map(|p| p.secret.clone())
                    .collect::<Vec<_>>()
                    == input_secrets
                && req.proofs.iter().all(|p| p.dleq.is_none())
                && req.commitment == commitment.commitment
                && req.content == commitment.body_content
                && secp256k1::SECP256K1
                    .verify_schnorr(
                        &req.wallet_signature,
                        &msg,
                        &ephemeral.x_only_public_key().0,
                    )
                    .is_ok()
        })
        .returning(move |_| {
            ProtestOutcome::from_parts(
                status,
                matches!(status, wire_common::ProtestStatus::Resolved)
                    .then(|| signatures.iter().cloned().map(Into::into).collect()),
            )
            .map_err(|e| Error::MintClientInternal(e.to_string()))
        });
    beta
}

async fn connect_recovery(
    s: &UnansweredLockSwap,
    alpha: MockClowderMintConnector,
    beta: MockClowderMintConnector,
) {
    let alpha: Arc<dyn ClowderMintConnector> = Arc::new(alpha);
    let beta: Arc<dyn ClowderMintConnector> = Arc::new(beta);
    let alpha_url = s.fx.alpha_url.clone();
    s.wlt.write().await.client_factory = Box::new(move |url| {
        if url == alpha_url {
            alpha.clone()
        } else {
            assert_eq!(url, url::Url::from_str("https://alpha-beta.test").unwrap());
            beta.clone()
        }
    });
}

async fn refund_after_locktime(s: &UnansweredLockSwap) -> usize {
    s.wlt
        .read()
        .await
        .refund_expired_exchanges(s.record.locktime + 1)
        .await
        .unwrap()
}

/// Asserts `s` was refunded as foreign mint proofs of its alpha, returning them
async fn assert_refunded(s: &UnansweredLockSwap, refunded: usize) -> Vec<cashu::Proof> {
    assert_eq!(refunded, 1);
    assert!(s.fx.pdb.list_exchange_records().await.unwrap().is_empty());
    assert!(s.fx.pdb.list_commitments().await.unwrap().is_empty());
    let stored = s.fx.pdb.load_foreign_mint_proofs().await.unwrap();
    for fmp in stored.iter() {
        assert_eq!(fmp.clowder_id, s.record.alpha_clowder_id);
        assert_eq!(
            fmp.reason,
            ForeignMintProofReason::ExchangeRefund {
                alpha_url: s.record.alpha_url.clone()
            }
        );
    }
    stored.into_iter().map(|fmp| fmp.proof).collect()
}

fn assert_locked_refund(s: &UnansweredLockSwap, proofs: &[cashu::Proof]) {
    let mut amounts: Vec<Amount> = proofs.iter().map(|p| p.amount).collect();
    amounts.sort();
    assert_eq!(amounts, vec![Amount::from(2), Amount::from(8)]);
    for proof in proofs {
        assert_eq!(htlc::htlc_lock(proof).unwrap().0, s.record.hash_lock);
        assert!(proof.witness.is_some());
    }
}

fn assert_alpha_inputs_refund(s: &UnansweredLockSwap, proofs: &[cashu::Proof]) {
    let mut refunded = secrets(proofs);
    let mut inputs = secrets(&s.record.alpha_inputs);
    refunded.sort_by_key(|secret| secret.to_string());
    inputs.sort_by_key(|secret| secret.to_string());
    assert_eq!(refunded, inputs);
}

#[tokio::test]
async fn exchange_lock_unanswered_dropped_response_is_protested_and_refunded() {
    let s = unanswered_lock_swap().await;
    let alpha = recovery_alpha(&s, false, Some(cashu::State::Spent));
    let beta = recovery_beta(&s, Some(wire_common::ProtestStatus::Resolved));
    connect_recovery(&s, alpha, beta).await;

    let refunded = refund_after_locktime(&s).await;
    assert_locked_refund(&s, &assert_refunded(&s, refunded).await);
}

#[tokio::test]
async fn exchange_lock_unanswered_restored_from_alpha_is_refunded() {
    let s = unanswered_lock_swap().await;
    let alpha = recovery_alpha(&s, true, None);
    let beta = recovery_beta(&s, None);
    connect_recovery(&s, alpha, beta).await;

    let refunded = refund_after_locktime(&s).await;
    assert_locked_refund(&s, &assert_refunded(&s, refunded).await);
}

#[tokio::test]
async fn exchange_lock_unanswered_never_executed_refunds_alpha_inputs() {
    let s = unanswered_lock_swap().await;
    let alpha = recovery_alpha(&s, false, Some(cashu::State::Unspent));
    let beta = recovery_beta(&s, None);
    connect_recovery(&s, alpha, beta).await;

    let refunded = refund_after_locktime(&s).await;
    assert_alpha_inputs_refund(&s, &assert_refunded(&s, refunded).await);
}

async fn assert_unanswered_lock_swap_kept(s: &UnansweredLockSwap, refunded: usize) {
    assert_eq!(refunded, 0);
    let records = s.fx.pdb.list_exchange_records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].locked_proofs.is_none());
    let commitments = s.fx.pdb.list_commitments().await.unwrap();
    assert_eq!(commitments.len(), 1);
    assert_eq!(commitments[0].commitment, s.commitment.commitment);
    assert!(
        s.fx.pdb
            .load_foreign_mint_proofs()
            .await
            .unwrap()
            .is_empty()
    );
}

type SentExchange = (
    Vec<bcr_common::wire::keys::ProofFingerprint>,
    Vec<Sha256>,
    secp256k1::PublicKey,
    secp256k1::schnorr::Signature,
);

struct MigrationCtx {
    pdb: Arc<bcr_wallet_persistence::redb::pocket::PocketDB>,
    substitute_url: url::Url,
    alpha_proofs: Vec<cashu::Proof>,
    beta_proofs: HashMap<Sha256, cashu::Proof>,
    beta_info: ecash::KeySetInfo,
    sent: Arc<std::sync::Mutex<Vec<SentExchange>>>,
    received: Arc<std::sync::Mutex<Vec<cashu::Proof>>>,
    lost: Arc<std::sync::Mutex<HashMap<cashu::PublicKey, bcr_common::client::mint::Error>>>,
    lock_secs: u64,
    lazy_keyset: bool,
}

impl MigrationCtx {
    async fn new(unspent: &[u64], held: &[u64]) -> Self {
        let pdb = Arc::new(
            bcr_wallet_persistence::test_utils::tests::in_memory_pocket_db(
                "w-1",
                CurrencyUnit::Sat,
            ),
        );
        let (_, alpha_keyset) = core_tests::generate_random_ecash_keyset();
        let amounts: Vec<_> = unspent
            .iter()
            .chain(held)
            .map(|a| Amount::from(*a))
            .collect();
        let mut alpha_proofs = core_tests::generate_random_ecash_proofs(&alpha_keyset, &amounts);
        add_test_dleqs(&mut alpha_proofs);
        let (beta_info, beta_keyset) = core_tests::generate_random_ecash_keyset();
        let mut beta_proofs = HashMap::new();
        for (i, alpha) in alpha_proofs.iter().enumerate() {
            if i < unspent.len() {
                pdb.store_new(alpha.clone()).await.unwrap();
            } else {
                pdb.store_pendingspent(alpha.clone()).await.unwrap();
            }
            let beta =
                core_tests::generate_random_ecash_proofs(&beta_keyset, &[alpha.amount]).remove(0);
            beta_proofs.insert(Sha256::hash(&alpha.secret.to_bytes()), beta);
        }
        Self {
            pdb,
            substitute_url: url::Url::from_str("https://substitute.example").unwrap(),
            alpha_proofs,
            beta_proofs,
            beta_info: beta_info.into(),
            sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            received: Arc::new(std::sync::Mutex::new(Vec::new())),
            lost: Arc::new(std::sync::Mutex::new(HashMap::new())),
            lock_secs: 3600,
            lazy_keyset: false,
        }
    }

    fn y(&self, i: usize) -> cashu::PublicKey {
        self.alpha_proofs[i].y().unwrap()
    }

    fn take_sent(&self) -> Vec<SentExchange> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }

    fn received(&self) -> Amount {
        self.received.lock().unwrap().total_amount().unwrap()
    }

    async fn states(&self) -> HashMap<cashu::PublicKey, MigrationJournalState> {
        let entries = self.pdb.load().await.unwrap();
        entries.into_iter().map(|(y, e)| (y, e.state)).collect()
    }

    fn substitute(&self, fail: Vec<cashu::PublicKey>, landed: bool) -> MockClowderMintConnector {
        self.substitute_at(
            self.substitute_url.clone(),
            test_other_pub_key(),
            [7u8; 32],
            fail,
            landed,
        )
    }

    async fn wallet_with_evidence(&self, evidence_digest: [u8; 32]) -> Arc<RwLock<Wallet>> {
        let wlt = self.wallet(vec![], 0, false).await;
        let substitute: Arc<dyn ClowderMintConnector> = Arc::new(self.substitute_at(
            self.substitute_url.clone(),
            test_other_pub_key(),
            evidence_digest,
            vec![],
            false,
        ));
        wlt.write().await.client_factory = Box::new(move |_| substitute.clone());
        wlt
    }

    fn substitute_at(
        &self,
        url: url::Url,
        clowder_id: secp256k1::PublicKey,
        evidence_digest: [u8; 32],
        fail: Vec<cashu::PublicKey>,
        landed: bool,
    ) -> MockClowderMintConnector {
        let mut substitute = MockClowderMintConnector::new();
        substitute
            .expect_get_clowder_id()
            .returning(move || Ok(clowder_id));
        let evidence = Arc::new(std::sync::Mutex::new(evidence_digest));
        let current = evidence.clone();
        substitute.expect_get_alpha_offline().returning(move |_| {
            Ok(wire_clowder::OfflineResponse {
                offline: true,
                evidence_digest: Some(*current.lock().unwrap()),
            })
        });
        let (beta_proofs, sent, lost) = (
            self.beta_proofs.clone(),
            self.sent.clone(),
            self.lost.clone(),
        );
        let held = std::sync::Mutex::new(
            HashMap::<cashu::PublicKey, ([u8; 32], Vec<cashu::Proof>)>::new(),
        );
        let lock_secs = self.lock_secs;
        let keyset_created = Arc::new(std::sync::atomic::AtomicBool::new(!self.lazy_keyset));
        let created = keyset_created.clone();
        substitute.expect_post_offline_exchange().returning(
            move |fingerprints, hashes, wallet_pk, signature, issuer| {
                created.store(true, std::sync::atomic::Ordering::SeqCst);
                let y: cashu::PublicKey = fingerprints[0].y.into();
                let hash = hashes[0];
                let verifies = |digest: &[u8; 32]| {
                    let exchange = bcr_common::wire::exchange::exchange_digest(
                        &test_pub_key(),
                        digest,
                        &fingerprints,
                        &hashes,
                        &wallet_pk.into(),
                    );
                    SECP256K1
                        .verify_schnorr(
                            &signature,
                            &bcr_common::wire::exchange::exchange_message(&exchange),
                            &wallet_pk.x_only_public_key().0,
                        )
                        .is_ok()
                };
                let refused = || {
                    bcr_common::client::mint::Error::InvalidRequest(
                        bcr_common::client::mint::BRError::Unknown,
                    )
                };
                let mut held = held.lock().unwrap();
                let mut evidence = evidence.lock().unwrap();
                let answer = match held.get(&y) {
                    Some((digest, proofs)) if verifies(digest) => Ok(proofs.clone()),
                    Some(_) => Err(refused()),
                    None if fail.contains(&y) || !verifies(&evidence) => Err(refused()),
                    None => {
                        let proofs = exchange_locked_for(
                            vec![beta_proofs[&hash].clone()],
                            &[hash],
                            wallet_pk,
                            issuer,
                            lock_secs,
                        );
                        held.insert(y, (*evidence, proofs.clone()));
                        match lost.lock().unwrap().remove(&y) {
                            Some(e) => {
                                evidence[0] = evidence[0].wrapping_add(1);
                                Err(e)
                            }
                            None => Ok(proofs),
                        }
                    }
                };
                sent.lock()
                    .unwrap()
                    .push((fingerprints, hashes, wallet_pk, signature));
                answer
            },
        );
        substitute.expect_get_clowder_betas().returning(|| {
            Ok(vec![ClowderBeta {
                url: url::Url::from_str("https://substitute-beta.example").unwrap(),
                clowder_id: test_pub_key(),
            }])
        });
        let beta_info = self.beta_info.clone();
        substitute.expect_get_mint_keysets().returning(move || {
            Ok(
                if keyset_created.load(std::sync::atomic::Ordering::SeqCst) {
                    vec![beta_info.clone()]
                } else {
                    vec![]
                },
            )
        });
        let state = if landed {
            cashu::State::Spent
        } else {
            cashu::State::Unspent
        };
        substitute.expect_post_check_state().returning(move |req| {
            Ok(req
                .ys
                .into_iter()
                .map(|y| cashu::ProofState {
                    y,
                    state,
                    witness: None,
                })
                .collect())
        });
        substitute.expect_mint_url().return_const(url);
        substitute
    }

    async fn wallet(
        &self,
        fail: Vec<cashu::PublicKey>,
        swap_failures: usize,
        landed: bool,
    ) -> Arc<RwLock<Wallet>> {
        let mut ctx = wallet_ctx();
        ctx.client
            .expect_mint_url()
            .return_const(url::Url::from_str("https://alpha.example").unwrap());
        let pocket_ys = self.pdb.list_all().await.unwrap();
        let mut payment = reclaimable_tx(Amount::ZERO);
        payment.ys = self.alpha_proofs.iter().map(|p| p.y().unwrap()).collect();
        ctx.tx_repo
            .expect_list_txs()
            .returning(move || Ok(vec![payment.clone()]));
        ctx.debit
            .expect_retain_migratable_proofs()
            .return_once(move |_| Ok(pocket_ys));
        ctx.debit.expect_set_beta_provider().returning(|_| ());
        ctx.debit.expect_unit().return_const(CurrencyUnit::Sat);
        ctx.nostr_transport.expect_relays().return_const(vec![]);
        let failures = Arc::new(std::sync::Mutex::new(swap_failures));
        let lost = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (l, received) = (lost.clone(), self.received.clone());
        ctx.debit
            .expect_receive_proofs()
            .returning(move |_, _, proofs, _| {
                let mut failures = failures.lock().unwrap();
                if *failures > 0 {
                    *failures -= 1;
                    if landed {
                        l.lock().unwrap().extend(proofs);
                    }
                    return Err(Error::Swap("connection reset".into()));
                }
                let amount = proofs.total_amount().unwrap();
                received.lock().unwrap().extend(proofs);
                Ok((amount, vec![]))
            });
        let received = self.received.clone();
        ctx.debit
            .expect_restore_local_proofs()
            .returning(move |_, _| {
                let restored = std::mem::take(&mut *lost.lock().unwrap());
                let n = restored.len();
                received.lock().unwrap().extend(restored);
                Ok(crate::pocket::RestoreSummary {
                    restored: n,
                    rejected: 0,
                })
            });
        ctx.debit
            .expect_balance()
            .returning(|_| Ok(PocketBalance::default()));
        let wlt = wallet(ctx).await;
        let substitute: Arc<dyn ClowderMintConnector> = Arc::new(self.substitute(fail, landed));
        {
            let mut w = wlt.write().await;
            w.journal = self.pdb.clone();
            w.client_factory = Box::new(move |_| substitute.clone());
        }
        wlt
    }

    async fn migrate(&self, wlt: &RwLock<Wallet>) -> Result<url::Url> {
        wlt.write()
            .await
            .migrate_pockets_substitute(self.substitute_url.clone())
            .await
    }
}

#[tokio::test]
async fn migrate_resumes_a_partial_exchange_with_the_identical_request() {
    let t = MigrationCtx::new(&[1, 2], &[4]).await;
    let wlt = t.wallet(vec![t.y(0)], 0, false).await;
    let err = t.migrate(&wlt).await.unwrap_err();
    assert!(matches!(err, Error::MigrationIncomplete(1)));
    assert_eq!(
        wlt.read().await.client.mint_url().as_str(),
        "https://alpha.example/"
    );
    assert_eq!(t.received(), Amount::ZERO);
    let first = t.take_sent();
    assert_eq!(first.len(), 2);
    let lost = first.into_iter().find(|s| s.0[0].y == *t.y(0)).unwrap();

    let wlt = t.wallet(vec![], 0, false).await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    assert_eq!(t.take_sent(), vec![lost]);
    assert_eq!(t.received(), Amount::from(3));
    assert!(
        t.received
            .lock()
            .unwrap()
            .iter()
            .all(|p| p.witness.is_some())
    );
    let states = t.states().await;
    assert_eq!(states[&t.y(0)], MigrationJournalState::Swapped);
    assert_eq!(states[&t.y(1)], MigrationJournalState::Swapped);
    assert_eq!(states[&t.y(2)], MigrationJournalState::Held);

    wlt.read().await.finish_migration().await.unwrap();
    assert_eq!(
        t.states().await.into_keys().collect::<Vec<_>>(),
        vec![t.y(2)]
    );
}

struct MovedElection {
    url: url::Url,
    clowder_id: secp256k1::PublicKey,
    evidence_digest: [u8; 32],
}

impl MovedElection {
    fn new() -> Self {
        Self {
            url: url::Url::from_str("https://moved-substitute.example").unwrap(),
            clowder_id: secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng())
                .public_key(),
            evidence_digest: [9u8; 32],
        }
    }

    /// A wallet whose betas now elect this substitute, while the one it journaled refuses `old_fails`
    async fn wallet(
        &self,
        t: &MigrationCtx,
        old_fails: Vec<cashu::PublicKey>,
    ) -> Arc<RwLock<Wallet>> {
        let wlt = t.wallet(vec![], 0, false).await;
        let old: Arc<dyn ClowderMintConnector> = Arc::new(t.substitute(old_fails, false));
        let moved: Arc<dyn ClowderMintConnector> = Arc::new(t.substitute_at(
            self.url.clone(),
            self.clowder_id,
            self.evidence_digest,
            vec![],
            false,
        ));
        let old_url = t.substitute_url.clone();
        wlt.write().await.client_factory = Box::new(move |url| {
            if url == old_url {
                old.clone()
            } else {
                moved.clone()
            }
        });
        wlt
    }

    async fn migrate(&self, wlt: &RwLock<Wallet>) -> Result<url::Url> {
        wlt.write()
            .await
            .migrate_pockets_substitute(self.url.clone())
            .await
    }
}

#[tokio::test]
async fn migration_follows_changed_substitute_before_any_exchange() {
    let t = MigrationCtx::new(&[1, 2], &[4]).await;
    let refused = vec![t.y(0), t.y(1)];
    let wlt = t.wallet(refused.clone(), 0, false).await;
    assert!(matches!(
        t.migrate(&wlt).await.unwrap_err(),
        Error::MigrationIncomplete(2)
    ));
    assert_eq!(t.take_sent().len(), 2);
    let journaled = t.pdb.load().await.unwrap();

    let moved = MovedElection::new();
    let wlt = moved.wallet(&t, refused).await;
    assert_eq!(moved.migrate(&wlt).await.unwrap(), moved.url);
    assert_eq!(t.take_sent().len(), 4);
    assert_eq!(t.received(), Amount::from(3));
    let reheaded = t.pdb.load().await.unwrap();
    assert_eq!(reheaded.len(), journaled.len());
    for (y, entry) in &reheaded {
        let before = &journaled[y];
        assert_eq!(before.header.substitute_url, t.substitute_url);
        assert_eq!(entry.header.substitute_url, moved.url);
        assert_eq!(entry.header.substitute_clowder_id, moved.clowder_id);
        assert_eq!(entry.header.evidence_digest, moved.evidence_digest);
        assert_eq!(entry.header.alpha_id, before.header.alpha_id);
        assert_eq!(entry.proof, before.proof);
        assert_eq!(entry.exchange_key, before.exchange_key);
    }
    assert_eq!(reheaded[&t.y(2)].state, MigrationJournalState::Held);
}

#[tokio::test]
async fn migration_follows_moved_evidence_before_any_exchange() {
    let t = MigrationCtx::new(&[1, 2], &[4]).await;
    let wlt = t.wallet(vec![t.y(0), t.y(1)], 0, false).await;
    assert!(matches!(
        t.migrate(&wlt).await.unwrap_err(),
        Error::MigrationIncomplete(2)
    ));
    let first = t.take_sent();
    let journaled = t.pdb.load().await.unwrap();

    let moved = [8u8; 32];
    let wlt = t.wallet_with_evidence(moved).await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    let sent = t.take_sent();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[..2], first[..]);
    assert_eq!(t.received(), Amount::from(3));
    let reheaded = t.pdb.load().await.unwrap();
    for (y, entry) in &reheaded {
        let before = &journaled[y];
        assert_eq!(before.header.evidence_digest, [7u8; 32]);
        assert_eq!(entry.header.evidence_digest, moved);
        assert_eq!(entry.header.substitute_url, before.header.substitute_url);
        assert_eq!(entry.exchange_key, before.exchange_key);
    }
    assert_eq!(reheaded[&t.y(2)].state, MigrationJournalState::Held);
}

#[tokio::test]
async fn migration_follows_moved_evidence_after_an_exchange() {
    let t = MigrationCtx::new(&[1, 2], &[]).await;
    let wlt = t.wallet(vec![t.y(0)], 0, false).await;
    assert!(matches!(
        t.migrate(&wlt).await.unwrap_err(),
        Error::MigrationIncomplete(1)
    ));
    assert!(matches!(
        t.states().await[&t.y(1)],
        MigrationJournalState::Exchanged(_)
    ));
    t.take_sent();

    let moved = [8u8; 32];
    let wlt = t.wallet_with_evidence(moved).await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    assert_eq!(t.take_sent().len(), 2);
    assert_eq!(t.received(), Amount::from(3));
    let entries = t.pdb.load().await.unwrap();
    assert!(
        entries
            .values()
            .all(|e| e.header.evidence_digest == moved
                && e.header.substitute_url == t.substitute_url)
    );
}

/// The substitute holds the exchange but its answer is lost while the alpha's evidence moves;
/// the next call recovers it with the identical request
async fn lost_answer_keeps_identical_request(lost: bcr_common::client::mint::Error) {
    let t = MigrationCtx::new(&[1], &[]).await;
    t.lost.lock().unwrap().insert(t.y(0), lost);
    let substitute: Arc<dyn ClowderMintConnector> = Arc::new(t.substitute(vec![], false));
    let wallet = async || {
        let wlt = t.wallet(vec![], 0, false).await;
        let substitute = substitute.clone();
        wlt.write().await.client_factory = Box::new(move |_| substitute.clone());
        wlt
    };

    let wlt = wallet().await;
    assert!(matches!(
        t.migrate(&wlt).await.unwrap_err(),
        Error::MigrationIncomplete(1)
    ));
    let first = t.take_sent();
    assert_eq!(first.len(), 1);
    let entries = t.pdb.load().await.unwrap();
    assert!(
        entries
            .values()
            .all(|e| e.header.evidence_digest == [7u8; 32])
    );

    let wlt = wallet().await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    assert_eq!(t.take_sent(), first);
    assert_eq!(t.received(), Amount::from(1));
}

#[tokio::test]
async fn migration_lost_answer_keeps_identical_request() {
    let unreachable = reqwest::get("http://127.0.0.1:1/").await.unwrap_err();
    lost_answer_keeps_identical_request(bcr_common::client::mint::Error::Reqwest(unreachable))
        .await;
}

#[tokio::test]
async fn migration_gateway_timeout_keeps_identical_request() {
    lost_answer_keeps_identical_request(bcr_common::client::mint::Error::Internal(
        "upstream request timeout".to_string(),
    ))
    .await;
}

#[tokio::test]
async fn migration_keeps_substitute_after_an_exchange() {
    let t = MigrationCtx::new(&[1, 2], &[]).await;
    let wlt = t.wallet(vec![t.y(0)], 0, false).await;
    assert!(matches!(
        t.migrate(&wlt).await.unwrap_err(),
        Error::MigrationIncomplete(1)
    ));
    t.take_sent();

    let moved = MovedElection::new();
    let wlt = moved.wallet(&t, vec![t.y(0)]).await;
    assert!(matches!(
        moved.migrate(&wlt).await.unwrap_err(),
        Error::MigrationIncomplete(1)
    ));
    assert_eq!(t.take_sent().len(), 1);
    let entries = t.pdb.load().await.unwrap();
    assert!(
        entries
            .values()
            .all(|e| e.header.substitute_url == t.substitute_url)
    );
    assert!(matches!(
        entries[&t.y(1)].state,
        MigrationJournalState::Exchanged(_)
    ));
}

#[tokio::test]
async fn exchange_lock_unanswered_kept_when_protest_unresolved() {
    let s = unanswered_lock_swap().await;
    let alpha = recovery_alpha(&s, false, Some(cashu::State::Spent));
    let beta = recovery_beta(&s, Some(wire_common::ProtestStatus::Rabid));
    connect_recovery(&s, alpha, beta).await;

    let refunded = refund_after_locktime(&s).await;
    assert_unanswered_lock_swap_kept(&s, refunded).await;
}

#[tokio::test]
async fn exchange_lock_unanswered_commitment_survives_expiry_until_locked() {
    let s = unanswered_lock_swap().await;
    let wlt = s.wlt.read().await;
    let check = || {
        wlt.debit
            .check_pending_commitments(u64::MAX, wlt.client.clone())
    };
    check().await.unwrap();
    assert_eq!(s.fx.pdb.list_commitments().await.unwrap().len(), 1);

    let mut record = s.record.clone();
    record.locked_proofs = Some(s.fx.alpha_proofs.clone());
    s.fx.pdb.store_exchange_record(record).await.unwrap();
    check().await.unwrap();
    assert!(s.fx.pdb.list_commitments().await.unwrap().is_empty());
}

async fn receive_through_online_exchange(store_succeeds: bool) -> Result<Uuid> {
    let mut ctx = wallet_ctx();
    let wallet_mint = url::Url::from_str("https://wallet-mint.test").unwrap();
    let alpha_url = url::Url::from_str("https://alpha.test").unwrap();
    let alpha_beta_url = url::Url::from_str("https://alpha-beta.test").unwrap();
    let (info, keyset) = core_tests::generate_random_ecash_keyset();
    let kid = info.id;
    let k_infos = test_kinfos(info);
    let alpha_proofs = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8)]);

    ctx.client
        .expect_mint_url()
        .return_const(wallet_mint.clone());
    ctx.client.expect_get_alpha_offline().returning(|_| {
        Ok(wire_clowder::OfflineResponse {
            offline: false,
            evidence_digest: None,
        })
    });
    ctx.client
        .expect_post_online_exchange()
        .times(1)
        .returning(|locked, _| Ok(locked));
    ctx.client
        .expect_get_mint_keysets()
        .returning(|| Ok(vec![]));
    ctx.tx_repo.expect_store_tx().returning(|tx| Ok(tx.id));
    ctx.debit.expect_unit().return_const(CurrencyUnit::Sat);

    let mut seq = mockall::Sequence::new();
    let keyset_for_lock = keyset.clone();
    ctx.debit
        .expect_lock_exchange()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_, _, _, _, _| {
            Ok(core_tests::generate_random_ecash_proofs(
                &keyset_for_lock,
                &[Amount::from(8)],
            ))
        });
    ctx.debit
        .expect_receive_proofs()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_, _, proofs, _| {
            if store_succeeds {
                Ok((
                    Amount::from(8),
                    proofs.iter().map(|p| p.y().unwrap()).collect(),
                ))
            } else {
                Err(Error::Swap("store failed".into()))
            }
        });
    ctx.debit
        .expect_delete_exchange_record()
        .times(usize::from(store_succeeds))
        .in_sequence(&mut seq)
        .returning(|_| Ok(()));

    let mut alpha_client = MockClowderMintConnector::new();
    let beta_url = alpha_beta_url.clone();
    alpha_client.expect_get_clowder_betas().returning(move || {
        Ok(vec![ClowderBeta {
            url: beta_url.clone(),
            clowder_id: test_pub_key(),
        }])
    });
    alpha_client
        .expect_get_mint_keysets()
        .returning(move || Ok(k_infos.values().cloned().collect()));
    alpha_client
        .expect_get_mint_keyset()
        .with(mockall::predicate::eq(kid))
        .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset, None)));
    let alpha_client: Arc<dyn ClowderMintConnector> = Arc::new(alpha_client);
    let mut alpha_beta = MockClowderMintConnector::new();
    setup_attestation_mock(&mut alpha_beta);
    let alpha_beta: Arc<dyn ClowderMintConnector> = Arc::new(alpha_beta);

    let wlt = wallet(ctx).await;
    let factory_alpha_url = alpha_url.clone();
    wlt.write().await.client_factory = Box::new(move |url| {
        if url == factory_alpha_url {
            alpha_client.clone()
        } else {
            alpha_beta.clone()
        }
    });

    let path = ConnectedMintsResponse {
        mints: vec![
            wire_clowder::ConnectedMintResponse {
                mint: alpha_url.clone(),
                clowder: url::Url::from_str("https://clowder-alpha.test").unwrap(),
                node_id: test_other_pub_key(),
            },
            wire_clowder::ConnectedMintResponse {
                mint: wallet_mint,
                clowder: url::Url::from_str("https://clowder-wallet.test").unwrap(),
                node_id: test_pub_key(),
            },
        ],
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
    wlt.read()
        .await
        ._receive_proofs(
            &HashMap::new(),
            alpha_proofs,
            CurrencyUnit::Sat,
            alpha_url,
            Some((path, HashMap::new())),
            now,
            None,
            PaymentType::Token,
            TransactionStatus::Settled,
            None,
            None,
            None,
        )
        .await
}

#[tokio::test]
async fn exchange_record_deleted_after_exchanged_proofs_are_stored() {
    receive_through_online_exchange(true)
        .await
        .expect("receive through online exchange works");
}

#[tokio::test]
async fn exchange_record_kept_when_exchanged_proofs_fail_to_store() {
    assert!(receive_through_online_exchange(false).await.is_err());
}

#[tokio::test]
async fn migrate_swaps_into_a_keyset_the_exchange_created() {
    let mut t = MigrationCtx::new(&[1, 2], &[]).await;
    t.lazy_keyset = true;
    let wlt = t.wallet(vec![], 0, false).await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    assert_eq!(t.received(), Amount::from(3));
}

#[tokio::test]
async fn migrate_resumes_a_failed_swap_without_a_second_exchange() {
    let t = MigrationCtx::new(&[8, 16], &[]).await;
    let wlt = t.wallet(vec![], 1, false).await;
    let err = t.migrate(&wlt).await.unwrap_err();
    assert!(matches!(err, Error::MigrationIncomplete(2)));
    assert_eq!(wlt.read().await.clowder_id, test_pub_key());
    assert_eq!(t.take_sent().len(), 2);
    assert_eq!(t.received(), Amount::ZERO);

    let wlt = t.wallet(vec![], 0, false).await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    assert!(t.take_sent().is_empty());
    assert_eq!(wlt.read().await.clowder_id, test_other_pub_key());
    assert_eq!(t.received(), Amount::from(24));
    wlt.read().await.finish_migration().await.unwrap();
    assert!(t.pdb.load().await.unwrap().is_empty());
}

#[tokio::test]
async fn migrate_resumes_issued_proofs_inside_the_hop_margin() {
    let mut t = MigrationCtx::new(&[8, 16], &[]).await;
    t.lock_secs = 300;
    let wlt = t.wallet(vec![], 1, false).await;
    let err = t.migrate(&wlt).await.unwrap_err();
    assert!(matches!(err, Error::MigrationIncomplete(2)));

    let wlt = t.wallet(vec![], 0, false).await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    assert_eq!(t.received(), Amount::from(24));
}

#[tokio::test]
async fn migrate_of_only_held_payments_is_found_again() {
    let t = MigrationCtx::new(&[], &[4]).await;
    let wlt = t.wallet(vec![], 0, false).await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    assert!(wlt.read().await.has_migration_entries().await.unwrap());
}

#[tokio::test]
async fn migrate_lost_swap_response_is_not_swapped_again() {
    let t = MigrationCtx::new(&[8], &[]).await;
    let wlt = t.wallet(vec![], 1, true).await;
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    assert_eq!(t.received(), Amount::from(8));
    assert_eq!(t.states().await[&t.y(0)], MigrationJournalState::Swapped);
}

#[tokio::test]
async fn reclaim_tx_reclaims_a_held_payment_at_the_substitute() {
    let t = MigrationCtx::new(&[8], &[4]).await;
    let wlt = t.wallet(vec![], 0, false).await;
    t.migrate(&wlt).await.unwrap();
    wlt.read().await.finish_migration().await.unwrap();
    assert_eq!(t.take_sent().len(), 1);

    let mut tx = reclaimable_tx(Amount::from(4));
    tx.ys = vec![t.y(1)];
    let tx_id = tx.id;
    let mut tx_repo = MockTransactionRepository::new();
    tx_repo.expect_load_tx().returning(move |_| Ok(tx.clone()));
    tx_repo.expect_store_tx().times(1).returning(|tx| Ok(tx.id));
    tx_repo
        .expect_update_status()
        .times(1)
        .returning(|_, _| Ok(None));
    tx_repo
        .expect_link_txs()
        .times(1)
        .returning(|_, _, _| Ok(()));
    wlt.write().await.tx_repo = Box::new(tx_repo);

    assert_eq!(
        wlt.read().await.reclaim_tx(tx_id).await.unwrap(),
        Amount::from(4)
    );
    let sent = t.take_sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0[0].y, *t.y(1));
    assert_eq!(t.received(), Amount::from(12));
    assert!(t.pdb.load().await.unwrap().is_empty());
}

#[tokio::test]
async fn migrate_again_while_a_payment_of_the_last_outage_is_held() {
    let t = MigrationCtx::new(&[8], &[4]).await;
    let wlt = t.wallet(vec![], 0, false).await;
    t.migrate(&wlt).await.unwrap();
    wlt.read().await.finish_migration().await.unwrap();
    t.take_sent();

    let wlt = t.wallet(vec![], 0, false).await;
    wlt.write().await.clowder_id = test_other_pub_key();
    assert_eq!(t.migrate(&wlt).await.unwrap(), t.substitute_url);
    wlt.read().await.finish_migration().await.unwrap();
    assert!(t.take_sent().is_empty());
    assert_eq!(
        t.states().await,
        HashMap::from([(t.y(1), MigrationJournalState::Held)])
    );
}

#[tokio::test]
async fn finish_migration_recovers_a_held_swap_input_that_no_payment_owns() {
    let t = MigrationCtx::new(&[8], &[4]).await;
    let wlt = t.wallet(vec![], 0, false).await;
    t.migrate(&wlt).await.unwrap();
    t.take_sent();
    let mut minted = reclaimable_tx(Amount::ZERO);
    minted.direction = TransactionDirection::Incoming;
    minted.status = TransactionStatus::Settled;
    minted.ys = t.alpha_proofs.iter().map(|p| p.y().unwrap()).collect();
    let mut tx_repo = MockTransactionRepository::new();
    tx_repo
        .expect_list_txs()
        .returning(move || Ok(vec![minted.clone()]));
    wlt.write().await.tx_repo = Box::new(tx_repo);
    wlt.read().await.finish_migration().await.unwrap();
    let sent = t.take_sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0[0].y, *t.y(1));
    assert_eq!(t.received(), Amount::from(12));
    assert!(t.pdb.load().await.unwrap().is_empty());
}

#[tokio::test]
async fn finish_migration_recovers_held_inputs_after_the_evidence_moved() {
    let t = MigrationCtx::new(&[8], &[4]).await;
    let wlt = t.wallet(vec![], 0, false).await;
    t.migrate(&wlt).await.unwrap();
    t.take_sent();
    let moved = [8u8; 32];
    let mut tx_repo = MockTransactionRepository::new();
    tx_repo.expect_list_txs().returning(|| Ok(vec![]));
    {
        let mut w = wlt.write().await;
        w.tx_repo = Box::new(tx_repo);
        w.client = Arc::new(t.substitute_at(
            t.substitute_url.clone(),
            test_other_pub_key(),
            moved,
            vec![],
            false,
        ));
    }
    wlt.read().await.finish_migration().await.unwrap();
    assert_eq!(t.take_sent().len(), 2);
    assert_eq!(t.received(), Amount::from(12));
    assert!(t.pdb.load().await.unwrap().is_empty());
}

#[tokio::test]
async fn finish_migration_retries_the_swap_of_an_exchanged_held_input() {
    let t = MigrationCtx::new(&[], &[4]).await;
    let wlt = t.wallet(vec![], 1, false).await;
    t.migrate(&wlt).await.unwrap();
    let mut tx_repo = MockTransactionRepository::new();
    tx_repo.expect_list_txs().returning(|| Ok(vec![]));
    wlt.write().await.tx_repo = Box::new(tx_repo);
    assert!(wlt.read().await.finish_migration().await.is_err());
    assert!(matches!(
        t.states().await[&t.y(0)],
        MigrationJournalState::Exchanged(_)
    ));
    assert_eq!(t.received(), Amount::ZERO);
    wlt.read().await.finish_migration().await.unwrap();
    assert_eq!(t.received(), Amount::from(4));
    assert!(t.pdb.load().await.unwrap().is_empty());
}

#[tokio::test]
async fn swap_exchanged_lost_response_already_restored_counts_as_swapped() {
    let (info, keyset) = core_tests::generate_random_ecash_keyset();
    let proofs = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8)]);
    let mut ctx = wallet_ctx();
    ctx.debit.expect_unit().return_const(CurrencyUnit::Sat);
    ctx.nostr_transport.expect_relays().return_const(vec![]);
    ctx.debit
        .expect_receive_proofs()
        .returning(|_, _, _, _| Err(Error::Swap("connection reset".into())));
    ctx.debit
        .expect_restore_local_proofs()
        .times(1)
        .returning(|_, _| Ok(crate::pocket::RestoreSummary::default()));
    let wlt = wallet(ctx).await;
    let mut substitute = MockClowderMintConnector::new();
    substitute.expect_post_check_state().returning(|req| {
        Ok(req
            .ys
            .into_iter()
            .map(|y| cashu::ProofState {
                y,
                state: cashu::State::Spent,
                witness: None,
            })
            .collect())
    });
    let info: ecash::KeySetInfo = info.into();
    let keysets_info = HashMap::from([(info.id, info)]);

    let w = wlt.read().await;
    let amount = w
        .swap_exchanged(Arc::new(substitute), &keysets_info, proofs, w.swap_config())
        .await
        .unwrap();
    assert_eq!(amount, Amount::from(8));
}
