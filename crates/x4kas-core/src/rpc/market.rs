use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock;

use crate::app::App;
use crate::rpc::types::MarketData;

const COINGECKO_URL: &str = "https://api.coingecko.com/api/v3/simple/price?ids=kaspa&vs_currencies=usd,btc&include_market_cap=true&include_24hr_vol=true&include_24hr_change=true";

#[derive(Debug, Deserialize)]
struct CoinGeckoResponse {
    kaspa: Option<KaspaPrice>,
}

#[derive(Debug, Default, Deserialize)]
struct KaspaPrice {
    usd: Option<f64>,
    btc: Option<f64>,
    usd_market_cap: Option<f64>,
    usd_24h_vol: Option<f64>,
    usd_24h_change: Option<f64>,
}

pub fn start_market_polling(app_state: Arc<RwLock<App>>, interval: Duration) {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("x4kas")
            .build()
            .unwrap();
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if app_state.read().await.paused {
                continue;
            }
            match fetch_market_data(&client).await {
                Ok(data) => {
                    let mut app = app_state.write().await;
                    app.market_data = Some(data);
                    app.market_error = None;
                    app.mark_dirty();
                }
                Err(e) => {
                    let mut app = app_state.write().await;
                    app.market_error = Some(e.to_string());
                    app.mark_dirty();
                }
            }
        }
    });
}

async fn fetch_market_data(client: &reqwest::Client) -> Result<MarketData, reqwest::Error> {
    let resp: CoinGeckoResponse = client.get(COINGECKO_URL).send().await?.json().await?;
    let kaspa = resp.kaspa.unwrap_or_default();
    Ok(MarketData {
        price_usd: kaspa.usd.unwrap_or(0.0),
        price_btc: kaspa.btc.unwrap_or(0.0),
        market_cap: kaspa.usd_market_cap.unwrap_or(0.0),
        volume_24h: kaspa.usd_24h_vol.unwrap_or(0.0),
        price_change_24h_pct: kaspa.usd_24h_change,
        fetched_at: Some(std::time::Instant::now()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_coingecko_response() {
        let json = r#"{
            "kaspa": {
                "usd": 0.15,
                "btc": 0.0000025,
                "usd_market_cap": 3800000000.0,
                "usd_24h_vol": 50000000.0,
                "usd_24h_change": 5.5
            }
        }"#;
        let resp: CoinGeckoResponse = serde_json::from_str(json).unwrap();
        let kaspa = resp.kaspa.unwrap();
        assert!((kaspa.usd.unwrap() - 0.15).abs() < f64::EPSILON);
        assert!((kaspa.btc.unwrap() - 0.0000025).abs() < f64::EPSILON);
        assert!((kaspa.usd_market_cap.unwrap() - 3_800_000_000.0).abs() < 1.0);
        assert!((kaspa.usd_24h_vol.unwrap() - 50_000_000.0).abs() < 1.0);
        assert!((kaspa.usd_24h_change.unwrap() - 5.5).abs() < f64::EPSILON);
    }

    #[test]
    fn deserialize_empty_response() {
        let json = r#"{}"#;
        let resp: CoinGeckoResponse = serde_json::from_str(json).unwrap();
        assert!(resp.kaspa.is_none());
    }

    #[test]
    fn deserialize_partial_response() {
        let json = r#"{"kaspa": {"usd": 0.10}}"#;
        let resp: CoinGeckoResponse = serde_json::from_str(json).unwrap();
        let kaspa = resp.kaspa.unwrap();
        assert!((kaspa.usd.unwrap() - 0.10).abs() < f64::EPSILON);
        assert!(kaspa.btc.is_none());
    }
}
