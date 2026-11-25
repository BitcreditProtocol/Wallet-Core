use tokio::sync::broadcast;
use uuid::Uuid;

const EVENT_BUFFER: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletEventKind {
    BalanceUpdated,
    PaymentReceived(Uuid),
    PaymentSent(Uuid),
    PaymentRequestReceived(Uuid),
    PaymentRequestUpdated(Uuid),
    TransactionUpdated(Uuid),
    WalletsChanged,
    /// Events were dropped for a slow receiver; its `wallet_id` is empty and it applies to every wallet, so per-wallet filters must let it through.
    Resync,
}

impl WalletEventKind {
    pub fn id(&self) -> Option<Uuid> {
        match *self {
            Self::PaymentReceived(id)
            | Self::PaymentSent(id)
            | Self::PaymentRequestReceived(id)
            | Self::PaymentRequestUpdated(id)
            | Self::TransactionUpdated(id) => Some(id),
            Self::BalanceUpdated | Self::WalletsChanged | Self::Resync => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletEvent {
    pub wallet_id: String,
    pub kind: WalletEventKind,
}

impl WalletEvent {
    pub fn from_recv(received: Result<Self, broadcast::error::RecvError>) -> Option<Self> {
        match received {
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(Self {
                wallet_id: String::new(),
                kind: WalletEventKind::Resync,
            }),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<WalletEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(EVENT_BUFFER).0,
        }
    }
}

impl EventBus {
    pub fn emit(&self, wallet_id: &str, kind: WalletEventKind) {
        let _ = self.tx.send(WalletEvent {
            wallet_id: wallet_id.to_owned(),
            kind,
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WalletEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::purse::tests::{purse, wlt_cfg};
    use crate::wallet::{api::MockWalletApi, types::WalletInfo};
    use bcr_common::core::NodeId;
    use bcr_wallet_persistence::MockPurseRepository;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn mock_wallet() -> MockWalletApi {
        let mut wlt = MockWalletApi::new();
        wlt.expect_id().returning(|| "wlt-1".to_owned());
        wlt.expect_config().returning(|| Ok(wlt_cfg()));
        wlt.expect_rename().return_const(());
        wlt.expect_delete().returning(|| Ok(()));
        wlt.expect_info().returning(|| WalletInfo {
            name: wlt_cfg().name,
            node_id: NodeId::new(wlt_cfg().pub_key, wlt_cfg().network),
            network: wlt_cfg().network,
            default_mint_url: wlt_cfg().mint,
            nostr_relays: vec![],
        });
        wlt
    }

    fn mock_repo() -> MockPurseRepository {
        let mut db = MockPurseRepository::new();
        db.expect_store().returning(|_| Ok(()));
        db.expect_rename().returning(|_, _| Ok(()));
        db.expect_delete().returning(|_| Ok(()));
        db
    }

    fn drain(rx: &mut broadcast::Receiver<WalletEvent>) -> Vec<WalletEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn wallets_changed(id: &str) -> WalletEvent {
        WalletEvent {
            wallet_id: id.to_owned(),
            kind: WalletEventKind::WalletsChanged,
        }
    }

    #[tokio::test]
    async fn wallets_changed_on_add_and_delete() {
        let purse = purse(Box::new(mock_repo()));
        let mut rx = purse.events.subscribe();
        purse
            .add_wallet(Arc::new(RwLock::new(mock_wallet())))
            .await
            .unwrap();
        purse.delete_wallet("wlt-1").await.unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![wallets_changed("wlt-1"), wallets_changed("wlt-1")]
        );
    }

    #[tokio::test]
    async fn delete_of_a_wallet_removed_meanwhile_emits_nothing() {
        let purse = purse(Box::new(mock_repo()));
        let wallets = purse.wallets.clone();
        let mut wlt = mock_wallet();
        wlt.checkpoint();
        wlt.expect_id().returning(|| "wlt-1".to_owned());
        wlt.expect_config().returning(|| Ok(wlt_cfg()));
        wlt.expect_delete().returning(move || {
            wallets.try_write().unwrap().remove("wlt-1");
            Ok(())
        });
        purse.add_wallet(Arc::new(RwLock::new(wlt))).await.unwrap();
        let mut rx = purse.events.subscribe();
        purse.delete_wallet("wlt-1").await.unwrap();
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn lagged_receiver_gets_a_resync_not_silence() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        bus.emit("w-0", WalletEventKind::BalanceUpdated);
        for _ in 0..EVENT_BUFFER {
            bus.emit("w-1", WalletEventKind::BalanceUpdated);
        }
        let first = WalletEvent::from_recv(rx.recv().await).unwrap();
        assert_eq!(first.kind, WalletEventKind::Resync);
        let next = WalletEvent::from_recv(rx.recv().await).unwrap();
        assert_eq!(next.wallet_id, "w-1");
        drop(bus);
        while rx.try_recv().is_ok() {}
        assert!(WalletEvent::from_recv(rx.recv().await).is_none());
    }

    #[tokio::test]
    async fn delete_with_repo_failure_after_wallet_delete_still_emits() {
        let mut db = mock_repo();
        db.checkpoint();
        db.expect_store().returning(|_| Ok(()));
        db.expect_delete().returning(|_| {
            Err(bcr_wallet_persistence::error::Error::BorshSerialization(
                "boom".into(),
            ))
        });
        let purse = purse(Box::new(db));
        purse
            .add_wallet(Arc::new(RwLock::new(mock_wallet())))
            .await
            .unwrap();
        let mut rx = purse.events.subscribe();
        assert!(purse.delete_wallet("wlt-1").await.is_err());
        assert_eq!(drain(&mut rx), vec![wallets_changed("wlt-1")]);
    }
}
