use std::str::FromStr;

use bcr_wallet_core::types::{BtcTxStatus, BtcTxStatusReceiver};
use bitcoin::Txid;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::try_join;

/// Generic result type
pub type Result<T> = std::result::Result<T, super::Error>;

/// Generic error type
#[derive(Debug, Error)]
pub enum Error {
    /// all errors originating from interacting with the web api
    #[error("External Bitcoin Web API error: {0}")]
    Api(#[from] reqwest::Error),

    /// all errors originating from dealing with invalid data from the API
    #[error("Got invalid data from the API")]
    InvalidData(String),
}

#[derive(Clone)]
pub struct BitcoinClient {
    cl: reqwest::Client,
    esplora_base_urls: Vec<url::Url>,
}

impl BitcoinClient {
    pub fn new(esplora_base_urls: Vec<url::Url>) -> Self {
        Self {
            cl: reqwest::Client::new(),
            esplora_base_urls,
        }
    }

    #[cfg(test)]
    pub fn with_urls(esplora_base_urls: Vec<url::Url>) -> Self {
        Self {
            cl: reqwest::Client::new(),
            esplora_base_urls,
        }
    }

    fn build_api_url(&self, base_url: &url::Url, path: &str, network: bitcoin::Network) -> String {
        match network {
            bitcoin::Network::Bitcoin => format!("{}api{path}", base_url),
            bitcoin::Network::Regtest => format!("{}regtest/api{path}", base_url),
            _ => format!("{}testnet/api{path}", base_url),
        }
    }

    fn is_retryable_error(error: &reqwest::Error) -> bool {
        if let Some(status) = error.status() {
            return status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT;
        }
        true
    }

    async fn request_with_fallback<T, F, P, Fut>(
        &self,
        path_builder: F,
        ctx: ReqContext,
        parse: P,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        F: Fn(&url::Url) -> String,
        P: Fn(reqwest::Response) -> Fut,
        Fut: Future<Output = std::result::Result<T, reqwest::Error>>,
    {
        let mut last_error: Option<Error> = None;

        for (i, base_url) in self.esplora_base_urls.iter().enumerate() {
            let url = path_builder(base_url);
            tracing::debug!(
                "Trying Esplora URL {}/{}: {}",
                i + 1,
                self.esplora_base_urls.len(),
                url
            );

            let call = match ctx {
                ReqContext::Get => self.cl.get(&url).send(),
                ReqContext::Post { ref payload } => self.cl.post(&url).body(payload.clone()).send(),
            };

            match call.await {
                Ok(response) => {
                    let status = response.status();

                    if status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || status == reqwest::StatusCode::REQUEST_TIMEOUT
                    {
                        tracing::warn!(
                            "Esplora URL {} returned retryable status {}, trying next",
                            base_url,
                            status
                        );
                        last_error = Some(Error::InvalidData(format!(
                            "HTTP {}: {}",
                            status.as_u16(),
                            status.canonical_reason().unwrap_or("Unknown")
                        )));
                        continue;
                    }

                    match response.error_for_status() {
                        Ok(res) => return parse(res).await.map_err(|e| Error::Api(e).into()),
                        Err(e) => return Err(Error::Api(e).into()),
                    };
                }
                Err(e) => {
                    if Self::is_retryable_error(&e) && i + 1 < self.esplora_base_urls.len() {
                        tracing::warn!(
                            "Esplora URL {} failed with retryable error: {}, trying next",
                            base_url,
                            e
                        );
                        last_error = Some(Error::Api(e));
                        continue;
                    }
                    return Err(Error::Api(e).into());
                }
            }
        }

        Err(last_error
            .expect("esplora_base_urls must not be empty")
            .into())
    }

    async fn get_tx(&self, txid: &Txid, network: bitcoin::Network) -> Result<Tx> {
        self.request_with_fallback(
            |base_url| self.build_api_url(base_url, &format!("/tx/{txid}"), network),
            ReqContext::Get,
            |response| async move { response.json::<Tx>().await },
        )
        .await
    }

    async fn get_last_block_height(&self, network: bitcoin::Network) -> Result<u64> {
        self.request_with_fallback(
            |base_url| self.build_api_url(base_url, "/blocks/tip/height", network),
            ReqContext::Get,
            |response| async move { response.json::<u64>().await },
        )
        .await
    }

    pub async fn check_status_for_transaction(
        &self,
        tx_id: Txid,
        network: bitcoin::Network,
    ) -> Result<BtcTxStatus> {
        let (chain_block_height, tx) = try_join!(
            self.get_last_block_height(network),
            self.get_tx(&tx_id, network)
        )?;
        let confirmations = if tx.status.confirmed
            && let Some(block_height) = tx.status.block_height
        {
            chain_block_height
                .checked_sub(block_height)
                .and_then(|difference| difference.checked_add(1))
                .unwrap_or(0)
        } else {
            0
        };
        let confirmation_tstamp = tx.status.block_time;

        let mut receivers = Vec::new();
        for vout in tx.vout {
            // only non-0 outputs of valid addresses
            if let Some(addr) = vout.scriptpubkey_address
                && vout.value > 0
                && let Ok(valid_addr) = bitcoin::Address::from_str(&addr)
            {
                receivers.push(BtcTxStatusReceiver {
                    address: valid_addr,
                    amount: bitcoin::Amount::from_sat(vout.value),
                })
            }
        }

        Ok(BtcTxStatus {
            tx_id,
            bitcoin_network: network,
            receivers,
            fee: bitcoin::Amount::from_sat(tx.fee.unwrap_or(0)),
            confirmations,
            confirmation_tstamp,
        })
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct Tx {
    #[allow(unused)]
    pub txid: String,
    pub status: Status,
    pub vout: Vec<Vout>,
    pub fee: Option<u64>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Vout {
    pub value: u64,
    pub scriptpubkey_address: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Status {
    pub block_height: Option<u64>,
    pub block_time: Option<u64>,
    #[allow(unused)]
    pub block_hash: Option<String>,
    pub confirmed: bool,
}

#[derive(Serialize, Debug, Clone)]
enum ReqContext {
    Get,
    #[allow(unused)]
    Post {
        payload: String,
    },
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fallback_on_server_error() {
        let mut server1 = mockito::Server::new_async().await;
        let mut server2 = mockito::Server::new_async().await;

        let m1 = server1
            .mock("GET", "/testnet/api/blocks/tip/height")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;

        let m2 = server2
            .mock("GET", "/testnet/api/blocks/tip/height")
            .with_status(200)
            .with_body("12345")
            .expect(1)
            .create_async()
            .await;

        let client = BitcoinClient::with_urls(vec![
            url::Url::parse(&server1.url()).unwrap(),
            url::Url::parse(&server2.url()).unwrap(),
        ]);

        let result: Result<u64> = client
            .request_with_fallback(
                |base_url| {
                    client.build_api_url(base_url, "/blocks/tip/height", bitcoin::Network::Testnet)
                },
                ReqContext::Get,
                |response| async move { response.json::<u64>().await },
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 12345);

        m1.assert_async().await;
        m2.assert_async().await;
    }

    #[tokio::test]
    async fn test_all_urls_fail() {
        let mut server1 = mockito::Server::new_async().await;
        let mut server2 = mockito::Server::new_async().await;

        let m1 = server1
            .mock("GET", "/testnet/api/blocks/tip/height")
            .with_status(503)
            .expect(1)
            .create_async()
            .await;

        let m2 = server2
            .mock("GET", "/testnet/api/blocks/tip/height")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;

        let client = BitcoinClient::with_urls(vec![
            url::Url::parse(&server1.url()).unwrap(),
            url::Url::parse(&server2.url()).unwrap(),
        ]);

        let result: Result<u64> = client
            .request_with_fallback(
                |base_url| {
                    client.build_api_url(base_url, "/blocks/tip/height", bitcoin::Network::Testnet)
                },
                ReqContext::Get,
                |response| async move { response.json::<u64>().await },
            )
            .await;

        assert!(result.is_err());

        m1.assert_async().await;
        m2.assert_async().await;
    }

    #[tokio::test]
    async fn test_primary_succeeds_no_fallback() {
        let mut server1 = mockito::Server::new_async().await;
        let mut server2 = mockito::Server::new_async().await;

        let m1 = server1
            .mock("GET", "/testnet/api/blocks/tip/height")
            .with_status(200)
            .with_body("99999")
            .expect(1)
            .create_async()
            .await;

        let m2 = server2
            .mock("GET", "/testnet/api/blocks/tip/height")
            .with_status(200)
            .with_body("11111")
            .expect(0)
            .create_async()
            .await;

        let client = BitcoinClient::with_urls(vec![
            url::Url::parse(&server1.url()).unwrap(),
            url::Url::parse(&server2.url()).unwrap(),
        ]);

        let result: Result<u64> = client
            .request_with_fallback(
                |base_url| {
                    client.build_api_url(base_url, "/blocks/tip/height", bitcoin::Network::Testnet)
                },
                ReqContext::Get,
                |response| async move { response.json::<u64>().await },
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 99999);

        m1.assert_async().await;
        m2.assert_async().await;
    }

    #[test]
    fn test_fallback_on_rate_limit() {
        let rt = ::tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut server1 = mockito::Server::new_async().await;
            let mut server2 = mockito::Server::new_async().await;

            let m1 = server1
                .mock("GET", "/testnet/api/blocks/tip/height")
                .with_status(429)
                .expect(1)
                .create_async()
                .await;

            let m2 = server2
                .mock("GET", "/testnet/api/blocks/tip/height")
                .with_status(200)
                .with_body("88888")
                .expect(1)
                .create_async()
                .await;

            let client = BitcoinClient::with_urls(vec![
                url::Url::parse(&server1.url()).unwrap(),
                url::Url::parse(&server2.url()).unwrap(),
            ]);

            let result: Result<u64> = client
                .request_with_fallback(
                    |base_url| {
                        client.build_api_url(
                            base_url,
                            "/blocks/tip/height",
                            bitcoin::Network::Testnet,
                        )
                    },
                    ReqContext::Get,
                    |response| async move { response.json::<u64>().await },
                )
                .await;

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 88888);

            m1.assert_async().await;
            m2.assert_async().await;
        });
    }

    #[test]
    fn test_fallback_on_request_timeout() {
        let rt = ::tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut server1 = mockito::Server::new_async().await;
            let mut server2 = mockito::Server::new_async().await;

            let m1 = server1
                .mock("GET", "/testnet/api/blocks/tip/height")
                .with_status(408)
                .expect(1)
                .create_async()
                .await;

            let m2 = server2
                .mock("GET", "/testnet/api/blocks/tip/height")
                .with_status(200)
                .with_body("77777")
                .expect(1)
                .create_async()
                .await;

            let client = BitcoinClient::with_urls(vec![
                url::Url::parse(&server1.url()).unwrap(),
                url::Url::parse(&server2.url()).unwrap(),
            ]);

            let result: Result<u64> = client
                .request_with_fallback(
                    |base_url| {
                        client.build_api_url(
                            base_url,
                            "/blocks/tip/height",
                            bitcoin::Network::Testnet,
                        )
                    },
                    ReqContext::Get,
                    |response| async move { response.json::<u64>().await },
                )
                .await;

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 77777);

            m1.assert_async().await;
            m2.assert_async().await;
        });
    }

    #[tokio::test]
    async fn test_check_status_for_confirmed_transaction() {
        let mut server = mockito::Server::new_async().await;

        let tx_id =
            Txid::from_str("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();

        let height_mock = server
            .mock("GET", "/testnet/api/blocks/tip/height")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("800000")
            .expect(1)
            .create_async()
            .await;

        let transaction_mock = server
        .mock(
            "GET",
            format!("/testnet/api/tx/{tx_id}").as_str(),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "txid": "1111111111111111111111111111111111111111111111111111111111111111",
                "status": {
                    "block_height": 799998,
                    "block_time": 1710000000,
                    "block_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "confirmed": true
                },
                "vout": [
                    {
                        "value": 50000,
                        "scriptpubkey_address": "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0"
                    },
                    {
                        "value": 0,
                        "scriptpubkey_address": "tb1qlzxh9zqzc0cfurkwjnua0ar0schh35f3836ngm"
                    },
                    {
                        "value": 123,
                        "scriptpubkey_address": "invalid_address"
                    },
                    {
                        "value": 25000,
                        "scriptpubkey_address": null
                    }
                ],
                "fee": 1500
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

        let client = BitcoinClient::with_urls(vec![url::Url::parse(&server.url()).unwrap()]);

        let status = client
            .check_status_for_transaction(tx_id, bitcoin::Network::Testnet)
            .await
            .unwrap();

        assert_eq!(status.tx_id, tx_id);
        assert_eq!(status.bitcoin_network, bitcoin::Network::Testnet);
        assert_eq!(status.confirmations, 3);
        assert_eq!(status.confirmation_tstamp, Some(1710000000));
        assert_eq!(status.fee, bitcoin::Amount::from_sat(1500));

        assert_eq!(status.receivers.len(), 1);
        assert_eq!(
            status.receivers[0].address.assume_checked_ref().to_string(),
            "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0"
        );
        assert_eq!(status.receivers[0].amount, bitcoin::Amount::from_sat(50000));

        height_mock.assert_async().await;
        transaction_mock.assert_async().await;
    }
}
