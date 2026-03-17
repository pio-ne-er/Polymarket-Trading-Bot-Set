//! CLOB market WebSocket client for real-time best bid/ask prices.
//! Connects to wss://ws-subscriptions-clob.polymarket.com/ws/market and subscribes to token IDs.
//! Updates a shared price cache on book, price_change, and best_bid_ask events.

use crate::monitor::MarketMonitor;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use log::{debug, warn};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const CLOB_WS_MARKET_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const PING_INTERVAL_SECS: u64 = 10;
const RECONNECT_DELAY_SECS: u64 = 5;
/// Check for new token IDs often so we resubscribe quickly when period changes (e.g. after update_markets).
const SUBSCRIPTION_REFRESH_INTERVAL_MS: u64 = 1000;

/// Shared cache: token_id -> (bid, ask). Best bid/ask from WebSocket.
pub type PriceCache = Arc<Mutex<HashMap<String, (Option<Decimal>, Option<Decimal>)>>>;

/// Spawn a background task that connects to the CLOB market WebSocket, subscribes to token IDs
/// from `monitor.get_current_token_ids()`, and updates `cache` on book / price_change / best_bid_ask.
/// Reconnects on disconnect.
pub fn spawn_clob_price_ws_task(cache: PriceCache, monitor: Arc<MarketMonitor>) {
    tokio::spawn(async move {
        loop {
            match run_clob_ws_loop(cache.clone(), monitor.clone()).await {
                Ok(()) => {}
                Err(e) => {
                    warn!(
                        "CLOB price WebSocket error: {} - reconnecting in {}s",
                        e, RECONNECT_DELAY_SECS
                    );
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
        }
    });
}

async fn run_clob_ws_loop(cache: PriceCache, monitor: Arc<MarketMonitor>) -> Result<()> {
    let (ws_stream, _) = connect_async(CLOB_WS_MARKET_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    let mut last_subscribed: Vec<String> = Vec::new();
    let mut last_ping = std::time::Instant::now();
    let mut last_sub_refresh = std::time::Instant::now();

    loop {
        // Refresh subscription periodically so we pick up new token IDs when period changes (update_markets clears IDs then refresh_market_tokens fills new ones).
        if last_sub_refresh.elapsed().as_millis() as u64 >= SUBSCRIPTION_REFRESH_INTERVAL_MS {
            let token_ids = monitor.get_current_token_ids().await;
            if token_ids != last_subscribed {
                if !token_ids.is_empty() {
                    // Unsubscribe from old assets first so the server stops sending them and we can cleanly subscribe to new period's tokens.
                    if !last_subscribed.is_empty() {
                        let unsubscribe = serde_json::json!({
                            "assets_ids": last_subscribed,
                            "operation": "unsubscribe"
                        });
                        if let Err(e) = write.send(Message::Text(unsubscribe.to_string())).await {
                            anyhow::bail!("send unsubscribe: {}", e);
                        }
                        debug!("CLOB WS unsubscribed from {} old token(s)", last_subscribed.len());
                        // Clear cache so we don't use stale prices from previous period.
                        let mut g = cache.lock().await;
                        g.clear();
                        drop(g);
                    }
                    // Subscribe: use "operation": "subscribe" when updating; use initial "type": "market" only on first subscription.
                    let subscribe = if last_subscribed.is_empty() {
                        serde_json::json!({
                            "assets_ids": token_ids,
                            "type": "market",
                            "custom_feature_enabled": true
                        })
                    } else {
                        serde_json::json!({
                            "assets_ids": token_ids,
                            "operation": "subscribe",
                            "custom_feature_enabled": true
                        })
                    };
                    if let Err(e) = write.send(Message::Text(subscribe.to_string())).await {
                        anyhow::bail!("send subscribe: {}", e);
                    }
                    last_subscribed = token_ids;
                    debug!("CLOB WS subscribed to {} token(s)", last_subscribed.len());
                } else {
                    // New period: monitor cleared token IDs (update_markets) but not yet refreshed. Clear last_subscribed so we'll subscribe when new IDs appear.
                    if !last_subscribed.is_empty() {
                        let unsubscribe = serde_json::json!({
                            "assets_ids": last_subscribed,
                            "operation": "unsubscribe"
                        });
                        if let Err(e) = write.send(Message::Text(unsubscribe.to_string())).await {
                            anyhow::bail!("send unsubscribe: {}", e);
                        }
                        debug!("CLOB WS unsubscribed from {} token(s) (new period, waiting for new IDs)", last_subscribed.len());
                        cache.lock().await.clear();
                    }
                    last_subscribed = Vec::new();
                }
            }
            last_sub_refresh = std::time::Instant::now();
        }

        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_ws_message(&cache, &text).await {
                            debug!("CLOB WS parse message: {}", e);
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Err(e)) => anyhow::bail!("ws read: {}", e),
                    Some(Ok(_)) => {}
                    None => anyhow::bail!("ws stream closed"),
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(500)) => {
                if last_ping.elapsed().as_secs() >= PING_INTERVAL_SECS {
                    if write.send(Message::Ping(vec![])).await.is_err() {
                        anyhow::bail!("ping send failed");
                    }
                    last_ping = std::time::Instant::now();
                }
            }
        }
    }
}

async fn handle_ws_message(cache: &PriceCache, text: &str) -> Result<()> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    let event_type = v
        .get("event_type")
        .and_then(|e| e.as_str())
        .ok_or_else(|| anyhow::anyhow!("no event_type"))?;

    match event_type {
        "book" => {
            let asset_id = v
                .get("asset_id")
                .and_then(|a| a.as_str())
                .ok_or_else(|| anyhow::anyhow!("book: no asset_id"))?
                .to_string();
            let best_bid = v
                .get("bids")
                .and_then(|b| b.as_array())
                .and_then(|b| b.first())
                .and_then(|b| b.get("price"))
                .and_then(|p| p.as_str())
                .and_then(|s| Decimal::from_str(s).ok());
            let best_ask = v
                .get("asks")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|a| a.get("price"))
                .and_then(|p| p.as_str())
                .and_then(|s| Decimal::from_str(s).ok());
            let mut g = cache.lock().await;
            g.insert(asset_id, (best_bid, best_ask));
        }
        "best_bid_ask" => {
            let asset_id = v
                .get("asset_id")
                .and_then(|a| a.as_str())
                .ok_or_else(|| anyhow::anyhow!("best_bid_ask: no asset_id"))?
                .to_string();
            let best_bid = v
                .get("best_bid")
                .and_then(|b| b.as_str())
                .and_then(|s| Decimal::from_str(s).ok());
            let best_ask = v
                .get("best_ask")
                .and_then(|a| a.as_str())
                .and_then(|s| Decimal::from_str(s).ok());
            let mut g = cache.lock().await;
            g.insert(asset_id, (best_bid, best_ask));
        }
        "price_change" => {
            let changes = v
                .get("price_changes")
                .and_then(|c| c.as_array())
                .ok_or_else(|| anyhow::anyhow!("price_change: no price_changes"))?;
            let mut g = cache.lock().await;
            for ch in changes {
                let asset_id = ch
                    .get("asset_id")
                    .and_then(|a| a.as_str())
                    .map(String::from);
                let best_bid = ch
                    .get("best_bid")
                    .and_then(|b| b.as_str())
                    .and_then(|s| Decimal::from_str(s).ok());
                let best_ask = ch
                    .get("best_ask")
                    .and_then(|a| a.as_str())
                    .and_then(|s| Decimal::from_str(s).ok());
                if let Some(id) = asset_id {
                    g.insert(id, (best_bid, best_ask));
                }
            }
        }
        _ => {}
    }
    Ok(())
}
