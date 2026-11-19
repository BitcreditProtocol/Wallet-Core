use tokio::sync::broadcast;

const EVENT_BUFFER: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletEventKind {
    BalanceUpdated,
    PaymentReceived,
    PaymentSent,
    PaymentRequestReceived,
    PaymentRequestUpdated,
    TransactionUpdated,
    WalletsChanged,
}

/// Informational notice that something of a wallet changed; receivers re-fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletEvent {
    pub wallet_id: String,
    pub kind: WalletEventKind,
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
        // no receivers is fine: events are informational
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
    use tokio::sync::{
        RwLock,
        broadcast::error::{RecvError, TryRecvError},
    };

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
    async fn wallets_changed_on_rename() {
        let purse = purse(Box::new(mock_repo()));
        purse
            .add_wallet(Arc::new(RwLock::new(mock_wallet())))
            .await
            .unwrap();
        let mut rx = purse.events.subscribe();
        purse
            .rename_wallet("wlt-1", "other".to_owned())
            .await
            .unwrap();
        assert_eq!(drain(&mut rx), vec![wallets_changed("wlt-1")]);
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
    async fn unknown_wallet_and_noop_rename_emit_nothing() {
        let purse = purse(Box::new(mock_repo()));
        purse
            .add_wallet(Arc::new(RwLock::new(mock_wallet())))
            .await
            .unwrap();
        let mut rx = purse.events.subscribe();
        assert!(purse.delete_wallet("missing").await.is_err());
        purse.rename_wallet("wlt-1", wlt_cfg().name).await.unwrap();
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn send_without_subscribers_ok() {
        let bus = EventBus::default();
        bus.emit("w-1", WalletEventKind::BalanceUpdated);
        let mut rx = bus.subscribe();
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn lagged_receiver_continues() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        for _ in 0..EVENT_BUFFER + 10 {
            bus.emit("w-1", WalletEventKind::BalanceUpdated);
        }
        assert!(matches!(rx.recv().await, Err(RecvError::Lagged(_))));
        bus.emit("w-1", WalletEventKind::WalletsChanged);
        let mut last = None;
        while let Ok(ev) = rx.try_recv() {
            last = Some(ev);
        }
        assert_eq!(last.unwrap().kind, WalletEventKind::WalletsChanged);
    }

    #[tokio::test]
    async fn every_subscriber_gets_the_event() {
        let bus = EventBus::default();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.emit("w-1", WalletEventKind::PaymentSent);
        for rx in [&mut a, &mut b] {
            let ev = rx.recv().await.unwrap();
            assert_eq!(ev.wallet_id, "w-1");
            assert_eq!(ev.kind, WalletEventKind::PaymentSent);
        }
    }
}
