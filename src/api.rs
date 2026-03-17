use crate::models::*;
use anyhow::{Context, Result};
use std::convert::TryFrom;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use hex;
use base64;
use log::{warn, info, error};
use std::sync::Arc;

// Official SDK imports for proper order signing
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{Side, OrderType, SignatureType, Amount};
use polymarket_client_sdk::{POLYGON, contract_config};
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use alloy::primitives::Address as AlloyAddress;

/// Cached authenticated CLOB client type (reused to avoid re-handshake on every order/balance call).
type ClobClientAuthenticated = ClobClient<Authenticated<Normal>>;

// CTF (Conditional Token Framework) imports for redemption
// Based on docs: https://docs.polymarket.com/developers/builders/relayer-client#redeem-positions
use alloy::primitives::{Address as AlloyAddressPrimitive, B256, TxKind, U256, Bytes};
use alloy::primitives::keccak256;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;

// Contract interfaces for direct RPC calls (like SDK example)
use alloy::sol;
use polymarket_client_sdk::types::Address;

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function allowance(address owner, address spender) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC1155 {
        function setApprovalForAll(address operator, bool approved) external;
        function isApprovedForAll(address account, address operator) external view returns (bool);
    }

    #[sol(rpc)]
    interface IConditionalTokens {
        function redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] indexSets) external;
    }
}

type HmacSha256 = Hmac<Sha256>;

pub struct PolymarketApi {
    client: Client,
    gamma_url: String,
    clob_url: String,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    private_key: Option<String>,
    // Proxy wallet configuration (for Polymarket proxy wallet)
    proxy_wallet_address: Option<String>,
    signature_type: Option<u8>, // 0 = EOA, 1 = Proxy, 2 = GnosisSafe
    // Track if authentication was successful at startup
    authenticated: Arc<tokio::sync::Mutex<bool>>,
    /// Cached authenticated CLOB client so we reuse connection/L2 auth instead of re-handshaking every order or balance call.
    clob_client_cache: Arc<tokio::sync::Mutex<Option<ClobClientAuthenticated>>>,
}

impl PolymarketApi {
    pub fn new(
        gamma_url: String,
        clob_url: String,
        api_key: Option<String>,
        api_secret: Option<String>,
        api_passphrase: Option<String>,
        private_key: Option<String>,
        proxy_wallet_address: Option<String>,
        signature_type: Option<u8>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");
        
        Self {
            client,
            gamma_url,
            clob_url,
            api_key,
            api_secret,
            api_passphrase,
            private_key,
            proxy_wallet_address,
            signature_type,
            authenticated: Arc::new(tokio::sync::Mutex::new(false)),
            clob_client_cache: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Returns a clone of the cached authenticated CLOB client, or creates and caches one. Reusing the client avoids a new TCP/TLS and L2 auth handshake on every order or balance call.
    async fn get_or_create_clob_client(&self) -> Result<ClobClientAuthenticated> {
        {
            let guard = self.clob_client_cache.lock().await;
            if let Some(ref c) = *guard {
                return Ok(c.clone());
            }
        }
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for CLOB. Set private_key in config.json"))?;
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key")?
            .with_chain_id(Some(POLYGON));
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context("Failed to parse proxy_wallet_address")?;
            auth_builder = auth_builder.funder(funder_address);
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) => anyhow::bail!("proxy_wallet_address set but signature_type 0 (EOA)"),
                None => SignatureType::Proxy,
                Some(n) => anyhow::bail!("Invalid signature_type: {}", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        let client = auth_builder
            .authenticate()
            .await
            .map_err(|e| anyhow::anyhow!("CLOB authenticate: {}", e))?;
        let mut guard = self.clob_client_cache.lock().await;
        *guard = Some(client.clone());
        Ok(client)
    }

    /// Clear cached CLOB client (e.g. after 401). Next call will re-authenticate.
    pub async fn clear_clob_client_cache(&self) {
        let mut guard = self.clob_client_cache.lock().await;
        *guard = None;
    }
    

    /// Authenticate with Polymarket CLOB API at startup
    /// This verifies credentials (private_key + API credentials)
    /// Equivalent to JavaScript: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress)
    pub async fn authenticate(&self) -> Result<()> {
        let _ = self.get_or_create_clob_client().await
            .context("Failed to authenticate with CLOB API. Check your API credentials (api_key, api_secret, api_passphrase) and private_key.")?;
        *self.authenticated.lock().await = true;
        
        eprintln!("✅ Successfully authenticated with Polymarket CLOB API (connection cached for faster orders)");
        eprintln!("   ✓ Private key: Valid");
        eprintln!("   ✓ API credentials: Valid");
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            eprintln!("   ✓ Proxy wallet: {}", proxy_addr);
        } else {
            eprintln!("   ✓ Trading account: EOA (private key account)");
        }
        Ok(())
    }

    /// Generate HMAC-SHA256 signature for authenticated requests
    fn generate_signature(
        &self,
        method: &str,
        path: &str,
        body: &str,
        timestamp: u64,
    ) -> Result<String> {
        let secret = self.api_secret.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API secret is required for authenticated requests"))?;
        
        // Create message: method + path + body + timestamp
        let message = format!("{}{}{}{}", method, path, body, timestamp);
        
        // Try to decode secret from base64url first (Builder API uses base64url encoding)
        // Base64url uses - and _ instead of + and /, making it URL-safe
        // Then try standard base64, then fall back to raw bytes
        let secret_bytes = {
            use base64::engine::general_purpose;
            use base64::Engine;
            
            // First try base64url (URL_SAFE engine)
            if let Ok(bytes) = general_purpose::URL_SAFE.decode(secret) {
                bytes
            }
            // Then try standard base64
            else if let Ok(bytes) = general_purpose::STANDARD.decode(secret) {
                bytes
            }
            // Finally, use raw bytes if both fail
            else {
                secret.as_bytes().to_vec()
            }
        };
        
        // Create HMAC-SHA256 signature
        let mut mac = HmacSha256::new_from_slice(&secret_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to create HMAC: {}", e))?;
        mac.update(message.as_bytes());
        let result = mac.finalize();
        let signature = hex::encode(result.into_bytes());
        
        Ok(signature)
    }

    /// Builder Relayer HMAC: message = timestamp(ms) + method + path + body, signature = base64url.
    /// Must match Polymarket builder-signing-sdk for relayer auth.
    fn builder_relayer_signature(
        api_secret: &str,
        timestamp_ms: u64,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<String> {
        let message = format!("{}{}{}{}", timestamp_ms, method, path, body);
        let secret_bytes = {
            use base64::engine::general_purpose;
            use base64::Engine;
            if let Ok(b) = general_purpose::URL_SAFE.decode(api_secret) {
                b
            } else if let Ok(b) = general_purpose::STANDARD.decode(api_secret) {
                b
            } else {
                api_secret.as_bytes().to_vec()
            }
        };
        let mut mac = HmacSha256::new_from_slice(&secret_bytes)
            .context("Failed to create HMAC for builder relayer")?;
        mac.update(message.as_bytes());
        let sig = mac.finalize().into_bytes();
        use base64::engine::general_purpose;
        use base64::Engine;
        Ok(general_purpose::URL_SAFE.encode(sig.as_slice()))
    }

    /// Add authentication headers to a request
    fn add_auth_headers(
        &self,
        request: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<reqwest::RequestBuilder> {
        // Only add auth headers if we have all required credentials
        if self.api_key.is_none() || self.api_secret.is_none() || self.api_passphrase.is_none() {
            return Ok(request);
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let signature = self.generate_signature(method, path, body, timestamp)?;
        
        let request = request
            .header("POLY_API_KEY", self.api_key.as_ref().unwrap())
            .header("POLY_SIGNATURE", signature)
            .header("POLY_TIMESTAMP", timestamp.to_string())
            .header("POLY_PASSPHRASE", self.api_passphrase.as_ref().unwrap());
        
        Ok(request)
    }

    /// Get all active markets (using events endpoint)
    pub async fn get_all_active_markets(&self, limit: u32) -> Result<Vec<Market>> {
        let url = format!("{}/events", self.gamma_url);
        let limit_str = limit.to_string();
        let mut params = HashMap::new();
        params.insert("active", "true");
        params.insert("closed", "false");
        params.insert("limit", &limit_str);

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch all active markets")?;

        let status = response.status();
        let json: Value = response.json().await.context("Failed to parse markets response")?;
        
        if !status.is_success() {
            log::warn!("Get all active markets API returned error status {}: {}", status, serde_json::to_string(&json).unwrap_or_default());
            anyhow::bail!("API returned error status {}: {}", status, serde_json::to_string(&json).unwrap_or_default());
        }
        
        // Extract markets from events - events contain markets
        let mut all_markets = Vec::new();
        
        if let Some(events) = json.as_array() {
            for event in events {
                if let Some(markets) = event.get("markets").and_then(|m| m.as_array()) {
                    for market_json in markets {
                        if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                            all_markets.push(market);
                        }
                    }
                }
            }
        } else if let Some(data) = json.get("data") {
            if let Some(events) = data.as_array() {
                for event in events {
                    if let Some(markets) = event.get("markets").and_then(|m| m.as_array()) {
                        for market_json in markets {
                            if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                                all_markets.push(market);
                            }
                        }
                    }
                }
            }
        }
        
        log::debug!("Fetched {} active markets from events endpoint", all_markets.len());
        Ok(all_markets)
    }

    /// Get market by slug (e.g., "btc-updown-15m-1767726000")
    /// The API returns an event object with a markets array
    pub async fn get_market_by_slug(&self, slug: &str) -> Result<Market> {
        let url = format!("{}/events/slug/{}", self.gamma_url, slug);
        
        let response = self.client.get(&url).send().await
            .context(format!("Failed to fetch market by slug: {}", slug))?;
        
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("Failed to fetch market by slug: {} (status: {})", slug, status);
        }
        
        let json: Value = response.json().await
            .context("Failed to parse market response")?;
        
        // The response is an event object with a "markets" array
        // Extract the first market from the markets array
        if let Some(markets) = json.get("markets").and_then(|m| m.as_array()) {
            if let Some(market_json) = markets.first() {
                // Try to deserialize the market
                if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                    return Ok(market);
                }
            }
        }
        
        anyhow::bail!("Invalid market response format: no markets array found")
    }

    /// Get order book for a specific token
    pub async fn get_orderbook(&self, token_id: &str) -> Result<OrderBook> {
        let url = format!("{}/book", self.clob_url);
        let params = [("token_id", token_id)];

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch orderbook")?;

        let orderbook: OrderBook = response
            .json()
            .await
            .context("Failed to parse orderbook")?;

        Ok(orderbook)
    }

    /// Get market details by condition ID
    pub async fn get_market(&self, condition_id: &str) -> Result<MarketDetails> {
        let url = format!("{}/markets/{}", self.clob_url, condition_id);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context(format!("Failed to fetch market for condition_id: {}", condition_id))?;

        let status = response.status();
        
        if !status.is_success() {
            anyhow::bail!("Failed to fetch market (status: {})", status);
        }

        let json_text = response.text().await
            .context("Failed to read response body")?;

        let market: MarketDetails = serde_json::from_str(&json_text)
            .map_err(|e| {
                log::error!("Failed to parse market response: {}. Response was: {}", e, json_text);
                anyhow::anyhow!("Failed to parse market response: {}", e)
            })?;

        Ok(market)
    }

    /// Get price for a token (for trading)
    /// side: "BUY" or "SELL"
    pub async fn get_price(&self, token_id: &str, side: &str) -> Result<rust_decimal::Decimal> {
        let url = format!("{}/price", self.clob_url);
        let params = [
            ("side", side),
            ("token_id", token_id),
        ];

        log::debug!("Fetching price from: {}?side={}&token_id={}", url, side, token_id);

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch price")?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("Failed to fetch price (status: {})", status);
        }

        let json: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse price response")?;

        let price_str = json.get("price")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid price response format"))?;

        let price = rust_decimal::Decimal::from_str(price_str)
            .context(format!("Failed to parse price: {}", price_str))?;

        log::debug!("Price for token {} (side={}): {}", token_id, side, price);

        Ok(price)
    }

    /// Get best bid/ask prices for a token (from orderbook)
    pub async fn get_best_price(&self, token_id: &str) -> Result<Option<TokenPrice>> {
        let orderbook = self.get_orderbook(token_id).await?;
        
        let best_bid = orderbook.bids.first().map(|b| b.price);
        let best_ask = orderbook.asks.first().map(|a| a.price);

        if best_ask.is_some() {
            Ok(Some(TokenPrice {
                token_id: token_id.to_string(),
                bid: best_bid,
                ask: best_ask,
            }))
        } else {
            Ok(None)
        }
    }

    /// Place an order using the official SDK with proper private key signing
    /// 
    /// This method uses the official polymarket-client-sdk to:
    /// 1. Create signer from private key
    /// 2. Authenticate with the CLOB API
    /// 3. Create and sign the order
    /// 4. Post the signed order
    /// 
    /// Equivalent to JavaScript: client.createAndPostOrder(userOrder)
    pub async fn place_order(&self, order: &OrderRequest) -> Result<OrderResponse> {
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for place_order")?;
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Convert order side string to SDK Side enum
        let side = match order.side.as_str() {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", order.side),
        };
        
        // Parse price and size to Decimal
        let price = rust_decimal::Decimal::from_str(&order.price)
            .context(format!("Failed to parse price: {}", order.price))?;
        let size = rust_decimal::Decimal::from_str(&order.size)
            .context(format!("Failed to parse size: {}", order.size))?;
        
        eprintln!("📤 Creating and posting order: {} {} {} @ {}", 
              order.side, order.size, order.token_id, order.price);
        
        // Create and post order using SDK (equivalent to: client.createAndPostOrder(userOrder))
        // This automatically creates, signs, and posts the order
        let order_builder = client
            .limit_order()
            .token_id(&order.token_id)
            .size(size)
            .price(price)
            .side(side);
        
        let signed_order = client.sign(&signer, order_builder.build().await?)
            .await
            .context("Failed to sign order")?;
        
        // Post order and capture detailed error information
        let response = match client.post_order(signed_order).await {
            Ok(resp) => resp,
            Err(e) => {
                // Log the full error details for debugging
                error!("❌ Failed to post order. Error details: {:?}", e);
                anyhow::bail!("Failed to post order: {}", e);
            }
        };
        
        // Check if the response indicates failure even if the request succeeded
        if !response.success {
            let error_msg = response.error_msg.as_deref().unwrap_or("Unknown error");
            error!("❌ Order rejected by API: {}", error_msg);
            anyhow::bail!(
                "Order was rejected: {}\n\
                Order details: Token ID={}, Side={}, Size={}, Price={}",
                error_msg, order.token_id, order.side, order.size, order.price
            );
        }
        
        // Convert SDK response to our OrderResponse format
        let order_response = OrderResponse {
            order_id: Some(response.order_id.clone()),
            status: response.status.to_string(),
            message: Some(format!("Order placed successfully. Order ID: {}", response.order_id)),
        };
        
        eprintln!("✅ Order placed successfully! Order ID: {}", response.order_id);
        
        Ok(order_response)
    }

    /// Place multiple limit orders in a single API request (batch).
    /// Returns one response per order in the same order. Each response has order_id/status/message;
    /// success is indicated by message starting with "Order ID:".
    pub async fn place_limit_orders(&self, orders: &[OrderRequest]) -> Result<Vec<OrderResponse>> {
        if orders.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for place_orders")?;
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing."))?;
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key")?
            .with_chain_id(Some(POLYGON));
        let mut signed_orders = Vec::with_capacity(orders.len());
        for order in orders.iter() {
            let side = match order.side.as_str() {
                "BUY" => Side::Buy,
                "SELL" => Side::Sell,
                _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", order.side),
            };
            let price = rust_decimal::Decimal::from_str(&order.price)
                .context(format!("Failed to parse price: {}", order.price))?;
            let size = rust_decimal::Decimal::from_str(&order.size)
                .context(format!("Failed to parse size: {}", order.size))?;
            let order_builder = client
                .limit_order()
                .token_id(&order.token_id)
                .size(size)
                .price(price)
                .side(side);
            let signed = client.sign(&signer, order_builder.build().await?)
                .await
                .context("Failed to sign order")?;
            signed_orders.push(signed);
        }
        eprintln!("📤 Posting {} limit orders in one batch...", signed_orders.len());
        let responses = client.post_orders(signed_orders).await
            .context("Failed to post batch limit orders")?;
        let mut out = Vec::with_capacity(responses.len());
        for (i, resp) in responses.into_iter().enumerate() {
            let token_id = orders.get(i).map(|o| o.token_id.as_str()).unwrap_or("?");
            out.push(OrderResponse {
                order_id: Some(resp.order_id.clone()),
                status: resp.status.to_string(),
                message: if resp.success {
                    Some(format!("Order ID: {}", resp.order_id))
                } else {
                    resp.error_msg.clone()
                },
            });
            if !resp.success {
                eprintln!("   Order {} (token {}...) rejected: {}", i + 1, &token_id[..token_id.len().min(16)], resp.error_msg.as_deref().unwrap_or("unknown"));
            }
        }
        Ok(out)
    }

    /// Cancel a specific order by order id (CLOB)
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for cancel_order")?;

        eprintln!("🛑 Cancelling order: {}", order_id);
        client.cancel_order(order_id).await
            .context(format!("Failed to cancel order {}", order_id))?;
        eprintln!("✅ Order cancel confirmed for order: {}", order_id);
        Ok(())
    }

    /// Fetch order by ID and return the filled size in shares (size_matched).
    /// This is the most exact source for "how much did this order fill": the exchange reports it immediately after match, with no chain/indexer delay.
    /// Returns None if the order cannot be fetched or size_matched is zero/invalid.
    pub async fn get_order_filled_shares(&self, order_id: &str) -> Result<Option<f64>> {
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for get_order")?;
        let order = client.order(order_id).await.context("Failed to fetch order")?;
        use rust_decimal::prelude::ToPrimitive;
        let shares = order.size_matched.to_f64().filter(|&s| s > 0.0);
        Ok(shares)
    }

    /// Discover current BTC or ETH 15-minute market
    /// Similar to main bot's discover_market function
    pub async fn discover_current_market(&self, asset: &str) -> Result<Option<String>> {
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        // Calculate current 15-minute period
        let current_period = (current_time / 900) * 900;
        
        // Try to find market for current period and a few previous periods (in case market is slightly delayed)
        for offset in 0..=2 {
            let period_to_check = current_period - (offset * 900);
            let slug = format!("{}-updown-15m-{}", asset.to_lowercase(), period_to_check);
            
            // Try to get market by slug
            if let Ok(market) = self.get_market_by_slug(&slug).await {
                return Ok(Some(market.condition_id));
            }
        }
        
        // If slug-based discovery fails, try searching active markets
        if let Ok(markets) = self.get_all_active_markets(50).await {
            let asset_upper = asset.to_uppercase();
            for market in markets {
                // Check if this is a BTC/ETH 15-minute market
                if market.slug.contains(&format!("{}-updown-15m", asset.to_lowercase())) 
                    || market.question.to_uppercase().contains(&format!("{} 15", asset_upper)) {
                    return Ok(Some(market.condition_id));
                }
            }
        }
        
        Ok(None)
    }

    /// Get all tokens in portfolio with balance > 0
    /// Get all tokens in portfolio with balance > 0, checking recent markets (not just current)
    /// Uses REDEEM_SCAN_PERIODS and a delay between requests to avoid 429 rate limits.
    /// Configured for BTC 5-minute markets (btc-updown-5m-{period}).
    pub async fn get_portfolio_tokens_all(&self, _btc_condition_id: Option<&str>, _eth_condition_id: Option<&str>) -> Result<Vec<(String, f64, String, String)>> {
        const PERIOD_SECS: u64 = 300; // 5-minute markets
        const REDEEM_SCAN_PERIODS: u32 = 72; // 72 × 5 min = 6 hours
        const DELAY_BETWEEN_PERIODS_MS: u64 = 200; // Delay between each period check to avoid rate limits
        let mut tokens_with_balance = Vec::new();
        
        // Check BTC 5-minute markets (current + previous periods)
        println!("🔍 Scanning BTC 5-minute markets (current + last {} periods ≈ 6h)...", REDEEM_SCAN_PERIODS);
        for offset in 0..=REDEEM_SCAN_PERIODS {
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let period_to_check = (current_time / PERIOD_SECS) * PERIOD_SECS - (offset as u64) * PERIOD_SECS;
            let slug = format!("btc-updown-5m-{}", period_to_check);
            
            if let Ok(market) = self.get_market_by_slug(&slug).await {
                let condition_id = market.condition_id.clone();
                println!("   📊 Checking BTC market: {} (period: {})", &condition_id[..16], period_to_check);
                
                if let Ok(market_details) = self.get_market(&condition_id).await {
                    for token in &market_details.tokens {
                        match self.check_balance_only(&token.token_id).await {
                            Ok(balance) => {
                                let balance_decimal = balance / rust_decimal::Decimal::from(1_000_000u64);
                                let balance_f64 = f64::try_from(balance_decimal).unwrap_or(0.0);
                                if balance_f64 > 0.0 {
                                    let description = format!("BTC {} (period: {})", token.outcome, period_to_check);
                                    tokens_with_balance.push((token.token_id.clone(), balance_f64, description, condition_id.clone()));
                                    println!("      ✅ Found token with balance: {} shares", balance_f64);
                                }
                            }
                            Err(_) => continue,
                        }
                    }
                }
            }
            // Delay between period checks to avoid 429 Too Many Requests (Cloudflare/relayer rate limit)
            if offset < REDEEM_SCAN_PERIODS {
                tokio::time::sleep(tokio::time::Duration::from_millis(DELAY_BETWEEN_PERIODS_MS)).await;
            }
        }
        
        // ETH and Solana scanning disabled temporarily (trading BTC only)
        // Uncomment the blocks below to re-enable.
        /*
        // Check ETH markets (current + recent past)
        println!("🔍 Scanning ETH markets (current + recent past)...");
        for offset in 0..=10 { // Check last 10 periods (2.5 hours)
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let period_to_check = (current_time / 900) * 900 - (offset * 900);
            let slug = format!("eth-updown-15m-{}", period_to_check);
            
            if let Ok(market) = self.get_market_by_slug(&slug).await {
                let condition_id = market.condition_id.clone();
                println!("   📊 Checking ETH market: {} (period: {})", &condition_id[..16], period_to_check);
                
                if let Ok(market_details) = self.get_market(&condition_id).await {
                    for token in &market_details.tokens {
                        match self.check_balance_only(&token.token_id).await {
                            Ok(balance) => {
                                let balance_decimal = balance / rust_decimal::Decimal::from(1_000_000u64);
                                let balance_f64 = f64::try_from(balance_decimal).unwrap_or(0.0);
                                if balance_f64 > 0.0 {
                                    let description = format!("ETH {} (period: {})", token.outcome, period_to_check);
                                    tokens_with_balance.push((token.token_id.clone(), balance_f64, description, condition_id.clone()));
                                    println!("      ✅ Found token with balance: {} shares", balance_f64);
                                }
                            }
                            Err(_) => continue,
                        }
                    }
                }
            }
        }
        
        // Check Solana markets (current + recent past)
        println!("🔍 Scanning Solana markets (current + recent past)...");
        for offset in 0..=10 { // Check last 10 periods (2.5 hours)
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let period_to_check = (current_time / 900) * 900 - (offset * 900);
            
            // Try both slug formats
            let slugs = vec![
                format!("solana-updown-15m-{}", period_to_check),
                format!("sol-updown-15m-{}", period_to_check),
            ];
            
            for slug in slugs {
                if let Ok(market) = self.get_market_by_slug(&slug).await {
                    let condition_id = market.condition_id.clone();
                    println!("   📊 Checking Solana market: {} (period: {})", &condition_id[..16], period_to_check);
                    
                    if let Ok(market_details) = self.get_market(&condition_id).await {
                        for token in &market_details.tokens {
                            match self.check_balance_only(&token.token_id).await {
                                Ok(balance) => {
                                    let balance_decimal = balance / rust_decimal::Decimal::from(1_000_000u64);
                                    let balance_f64 = f64::try_from(balance_decimal).unwrap_or(0.0);
                                    if balance_f64 > 0.0 {
                                        let description = format!("Solana {} (period: {})", token.outcome, period_to_check);
                                        tokens_with_balance.push((token.token_id.clone(), balance_f64, description, condition_id.clone()));
                                        println!("      ✅ Found token with balance: {} shares", balance_f64);
                                    }
                                }
                                Err(_) => continue,
                            }
                        }
                    }
                    break; // Found a valid market, no need to try other slug format
                }
            }
        }
        */
        
        Ok(tokens_with_balance)
    }

    /// Automatically discovers current BTC and ETH markets if condition IDs are not provided
    pub async fn get_portfolio_tokens(&self, btc_condition_id: Option<&str>, eth_condition_id: Option<&str>) -> Result<Vec<(String, f64, String)>> {
        let mut tokens_with_balance = Vec::new();
        
        // Discover BTC market if not provided
        let btc_condition_id_owned: Option<String> = if let Some(id) = btc_condition_id {
            Some(id.to_string())
        } else {
            println!("🔍 Discovering current BTC 15-minute market...");
            match self.discover_current_market("BTC").await {
                Ok(Some(id)) => {
                    println!("   ✅ Found BTC market: {}", id);
                    Some(id)
                }
                Ok(None) => {
                    println!("   ⚠️  Could not find current BTC market");
                    None
                }
                Err(e) => {
                    eprintln!("   ❌ Error discovering BTC market: {}", e);
                    None
                }
            }
        };
        
        // Discover ETH market if not provided
        let eth_condition_id_owned: Option<String> = if let Some(id) = eth_condition_id {
            Some(id.to_string())
        } else {
            println!("🔍 Discovering current ETH 15-minute market...");
            match self.discover_current_market("ETH").await {
                Ok(Some(id)) => {
                    println!("   ✅ Found ETH market: {}", id);
                    Some(id)
                }
                Ok(None) => {
                    println!("   ⚠️  Could not find current ETH market");
                    None
                }
                Err(e) => {
                    eprintln!("   ❌ Error discovering ETH market: {}", e);
                    None
                }
            }
        };
        
        // Check BTC market tokens
        if let Some(ref btc_condition_id) = btc_condition_id_owned {
            println!("📊 Checking BTC market tokens for condition: {}", btc_condition_id);
            if let Ok(btc_market) = self.get_market(btc_condition_id).await {
                println!("   ✅ Found {} tokens in BTC market", btc_market.tokens.len());
                for token in &btc_market.tokens {
                    println!("   🔍 Checking balance for token: {} ({})", token.outcome, &token.token_id[..16]);
                    match self.check_balance_allowance(&token.token_id).await {
                        Ok((balance, _)) => {
                            let balance_decimal = balance / rust_decimal::Decimal::from(1_000_000u64);
                            let balance_f64 = f64::try_from(balance_decimal).unwrap_or(0.0);
                            println!("      Balance: {:.6} shares", balance_f64);
                            if balance_f64 > 0.0 {
                                tokens_with_balance.push((token.token_id.clone(), balance_f64, format!("BTC {}", token.outcome)));
                                println!("      ✅ Found token with balance!");
                            }
                        }
                        Err(e) => {
                            println!("      ⚠️  Failed to check balance: {}", e);
                            // Skip tokens that fail balance check (might not exist or network error)
                            continue;
                        }
                    }
                }
            } else {
                eprintln!("   ❌ Failed to fetch BTC market details");
            }
        }
        
        // Check ETH market tokens
        if let Some(ref eth_condition_id) = eth_condition_id_owned {
            println!("📊 Checking ETH market tokens for condition: {}", eth_condition_id);
            if let Ok(eth_market) = self.get_market(eth_condition_id).await {
                println!("   ✅ Found {} tokens in ETH market", eth_market.tokens.len());
                for token in &eth_market.tokens {
                    println!("   🔍 Checking balance for token: {} ({})", token.outcome, &token.token_id[..16]);
                    match self.check_balance_allowance(&token.token_id).await {
                        Ok((balance, _)) => {
                            let balance_decimal = balance / rust_decimal::Decimal::from(1_000_000u64);
                            let balance_f64 = f64::try_from(balance_decimal).unwrap_or(0.0);
                            println!("      Balance: {:.6} shares", balance_f64);
                            if balance_f64 > 0.0 {
                                tokens_with_balance.push((token.token_id.clone(), balance_f64, format!("ETH {}", token.outcome)));
                                println!("      ✅ Found token with balance!");
                            }
                        }
                        Err(e) => {
                            println!("      ⚠️  Failed to check balance: {}", e);
                            // Skip tokens that fail balance check
                            continue;
                        }
                    }
                }
            } else {
                eprintln!("   ❌ Failed to fetch ETH market details");
            }
        }
        
        Ok(tokens_with_balance)
    }

    /// Check USDC balance and allowance for buying tokens
    /// Returns (usdc_balance, usdc_allowance) as Decimal values
    /// For BUY orders, you need USDC balance and USDC allowance to the Exchange contract
    pub async fn check_usdc_balance_allowance(&self) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal)> {
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for USDC balance check")?;
        
        // For USDC (collateral/ERC20), the API requires asset_id to be empty ("erc20 operation, asset must be empty").
        use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
        use polymarket_client_sdk::clob::types::AssetType;
        let request = BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Collateral)
            .build();
        
        let balance_allowance = client
            .balance_allowance(request)
            .await
            .context("Failed to fetch USDC balance and allowance")?;
        
        let balance = balance_allowance.balance;
        // Get allowance for the Exchange contract
        let config = contract_config(POLYGON, false)
            .ok_or_else(|| anyhow::anyhow!("Failed to get contract config"))?;
        let exchange_address = config.exchange;
        
        // Allowances is a HashMap<Address, String> - get the allowance for the Exchange contract
        let allowance = balance_allowance.allowances
            .get(&exchange_address)
            .and_then(|s| rust_decimal::Decimal::from_str(s).ok())
            .unwrap_or(rust_decimal::Decimal::ZERO);
        
        Ok((balance, allowance))
    }

    /// Check token balance only (for redemption/portfolio scanning)
    /// Returns balance as Decimal value
    /// This is faster than check_balance_allowance since it doesn't check allowances
    pub async fn check_balance_only(&self, token_id: &str) -> Result<rust_decimal::Decimal> {
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for balance check")?;
        
        // Get balance using SDK (only balance, not allowance)
        use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
        use polymarket_client_sdk::clob::types::AssetType;
        
        let request = BalanceAllowanceRequest::builder()
            .token_id(token_id.to_string())
            .asset_type(AssetType::Conditional)
            .build();
        
        let balance_allowance = client
            .balance_allowance(request)
            .await
            .context("Failed to fetch balance")?;
        
        Ok(balance_allowance.balance)
    }

    /// Like check_balance_only but retries up to max_attempts with delay_secs between attempts.
    /// Waits initial_wait_secs before the first attempt (e.g. for chain to settle after a fill).
    /// Returns the maximum balance seen across attempts to avoid understating (e.g. indexer delay).
    pub async fn check_balance_only_with_retry(
        &self,
        token_id: &str,
        initial_wait_secs: u64,
        max_attempts: u32,
        delay_secs: u64,
    ) -> Result<rust_decimal::Decimal> {
        tokio::time::sleep(tokio::time::Duration::from_secs(initial_wait_secs)).await;
        let mut max_balance = rust_decimal::Decimal::ZERO;
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            if attempt > 1 {
                tokio::time::sleep(tokio::time::Duration::from_secs(delay_secs)).await;
            }
            match self.check_balance_only(token_id).await {
                Ok(b) => {
                    if b > max_balance {
                        max_balance = b;
                    }
                }
                Err(e) => {
                    last_error = Some(e);
                    log::debug!("Balance check attempt {}/{} failed: {}", attempt, max_attempts, last_error.as_ref().unwrap());
                }
            }
        }
        if max_balance > rust_decimal::Decimal::ZERO {
            Ok(max_balance)
        } else if let Some(e) = last_error {
            Err(anyhow::anyhow!("Balance check failed after {} attempts: {}", max_attempts, e))
        } else {
            Ok(max_balance)
        }
    }

    /// Check token balance and allowance before selling
    /// Returns (balance, allowance) as Decimal values
    pub async fn check_balance_allowance(&self, token_id: &str) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal)> {
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for balance/allowance check")?;
        
        // Get balance and allowance using SDK
        // The SDK requires a BalanceAllowanceRequest built with builder pattern
        use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
        use polymarket_client_sdk::clob::types::AssetType;
        
        // Build the request: BalanceAllowanceRequest::builder().token_id(token_id).asset_type(...).build()
        // Conditional tokens are AssetType::Conditional
        let request = BalanceAllowanceRequest::builder()
            .token_id(token_id.to_string())
            .asset_type(AssetType::Conditional)
            .build();
        
        let balance_allowance = client
            .balance_allowance(request)
            .await
            .context("Failed to fetch balance and allowance")?;
        
        let balance = balance_allowance.balance;
        
        // Get contract config to check which contract address we should be checking allowance for
        let config = contract_config(POLYGON, false)
            .ok_or_else(|| anyhow::anyhow!("Failed to get contract config"))?;
        let exchange_address = config.exchange;
        
        // Get allowance for the Exchange contract specifically
        let allowance = balance_allowance.allowances
            .get(&exchange_address)
            .and_then(|v| rust_decimal::Decimal::from_str(v).ok())
            .unwrap_or_else(|| {
                // If Exchange contract not found, try to get any allowance (fallback)
                balance_allowance.allowances
                    .values()
                    .next()
                    .and_then(|v| rust_decimal::Decimal::from_str(v).ok())
                    .unwrap_or(rust_decimal::Decimal::ZERO)
            });

        Ok((balance, allowance))
    }

    /// Refresh cached allowance data for a specific outcome token before selling.
    /// 
    /// Per Polymarket: setApprovalForAll() is general approval, but for selling you need
    /// CTF (outcome tokens) approval for CTF Exchange tracked **per token**. The system
    /// caches allowances per token. Calling update_balance_allowance refreshes the backend's
    /// cached allowance for this specific token, reducing "insufficient allowance" errors
    /// when placing the sell order immediately after.
    /// 
    /// Call this right before place_market_order(..., "SELL", ...) for the token you're selling.
    pub async fn update_balance_allowance_for_sell(&self, token_id: &str) -> Result<()> {
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for update_balance_allowance")?;
        
        use polymarket_client_sdk::clob::types::request::UpdateBalanceAllowanceRequest;
        use polymarket_client_sdk::clob::types::AssetType;
        
        // Outcome tokens (conditional tokens) need AssetType::Conditional
        let request = UpdateBalanceAllowanceRequest::builder()
            .token_id(token_id.to_string())
            .asset_type(AssetType::Conditional)
            .build();
        
        client
            .update_balance_allowance(request)
            .await
            .context("Failed to update balance/allowance cache for token")?;
        
        Ok(())
    }

    /// Get the CLOB contract address for Polygon using SDK's contract_config
    /// This is the Exchange contract address that needs to be approved via setApprovalForAll
    fn get_clob_contract_address(&self) -> Result<String> {
        // Use SDK's contract_config to get the correct Exchange contract address
        let config = contract_config(POLYGON, false)
            .ok_or_else(|| anyhow::anyhow!("Failed to get contract config from SDK"))?;
        Ok(format!("{:#x}", config.exchange))
    }

    /// Get the CTF contract address for Polygon using SDK's contract_config
    /// This is where we call setApprovalForAll()
    fn get_ctf_contract_address(&self) -> Result<String> {
        // Use SDK's contract_config to get the correct CTF contract address
        let config = contract_config(POLYGON, false)
            .ok_or_else(|| anyhow::anyhow!("Failed to get contract config from SDK"))?;
        Ok(format!("{:#x}", config.conditional_tokens))
    }

    /// Check if setApprovalForAll was already set for the Exchange contract
    /// Returns true if the Exchange is already approved to manage all tokens
    pub async fn check_is_approved_for_all(&self) -> Result<bool> {
        let config = contract_config(POLYGON, false)
            .ok_or_else(|| anyhow::anyhow!("Failed to get contract config from SDK"))?;
        
        let ctf_contract_address = config.conditional_tokens;
        let exchange_address = config.exchange;
        
        // Determine which address to check (proxy wallet or EOA)
        let account_to_check = if let Some(proxy_addr) = &self.proxy_wallet_address {
            AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}", proxy_addr))?
        } else {
            let private_key = self.private_key.as_ref()
                .ok_or_else(|| anyhow::anyhow!("Private key required to check approval"))?;
            let signer = LocalSigner::from_str(private_key)
                .context("Failed to create signer from private key")?
                .with_chain_id(Some(POLYGON));
            signer.address()
        };
        
        const RPC_URL: &str = "https://polygon-rpc.com";
        let provider = ProviderBuilder::new()
            .connect(RPC_URL)
            .await
            .context("Failed to connect to Polygon RPC")?;
        
        let ctf = IERC1155::new(ctf_contract_address, provider);
        
        let approved = ctf
            .isApprovedForAll(account_to_check, exchange_address)
            .call()
            .await
            .context("Failed to check isApprovedForAll")?;
        
        Ok(approved)
    }

    /// Check all approvals for all contracts (like SDK's check_approvals example)
    /// Returns a vector of (contract_name, usdc_approved, ctf_approved) tuples
    pub async fn check_all_approvals(&self) -> Result<Vec<(String, bool, bool)>> {
        use polymarket_client_sdk::types::address;
        
        const RPC_URL: &str = "https://polygon-rpc.com";
        const USDC_ADDRESS: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
        
        let config = contract_config(POLYGON, false)
            .ok_or_else(|| anyhow::anyhow!("Failed to get contract config from SDK"))?;
        let neg_risk_config = contract_config(POLYGON, true)
            .ok_or_else(|| anyhow::anyhow!("Failed to get neg risk contract config from SDK"))?;
        
        // Determine which address to check (proxy wallet or EOA)
        let account_to_check = if let Some(proxy_addr) = &self.proxy_wallet_address {
            AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}", proxy_addr))?
        } else {
            let private_key = self.private_key.as_ref()
                .ok_or_else(|| anyhow::anyhow!("Private key required to check approval"))?;
            let signer = LocalSigner::from_str(private_key)
                .context("Failed to create signer from private key")?
                .with_chain_id(Some(POLYGON));
            signer.address()
        };
        
        let provider = ProviderBuilder::new()
            .connect(RPC_URL)
            .await
            .context("Failed to connect to Polygon RPC")?;
        
        let usdc = IERC20::new(USDC_ADDRESS, provider.clone());
        let ctf = IERC1155::new(config.conditional_tokens, provider.clone());
        
        // Collect all contracts that need approval
        let mut targets: Vec<(&str, Address)> = vec![
            ("CTF Exchange", config.exchange),
            ("Neg Risk CTF Exchange", neg_risk_config.exchange),
        ];
        
        if let Some(adapter) = neg_risk_config.neg_risk_adapter {
            targets.push(("Neg Risk Adapter", adapter));
        }
        
        let mut results = Vec::new();
        
        for (name, target) in &targets {
            let usdc_approved = usdc
                .allowance(account_to_check, *target)
                .call()
                .await
                .map(|allowance| allowance > U256::ZERO)
                .unwrap_or(false);
            
            let ctf_approved = ctf
                .isApprovedForAll(account_to_check, *target)
                .call()
                .await
                .unwrap_or(false);
            
            results.push((name.to_string(), usdc_approved, ctf_approved));
        }
        
        Ok(results)
    }

    /// Approve the CLOB contract for ALL conditional tokens using CTF contract's setApprovalForAll()
    /// This is the recommended way to avoid allowance errors for all tokens at once
    /// Based on SDK example: https://github.com/Polymarket/rs-clob-client/blob/main/examples/approvals.rs
    /// 
    /// For proxy wallets: Uses Polymarket's relayer to execute the transaction (gasless)
    /// For EOA wallets: Uses direct RPC call
    /// 
    /// IMPORTANT: The wallet that needs MATIC for gas:
    /// - If using proxy_wallet_address: Uses relayer (gasless, no MATIC needed)
    /// - If NOT using proxy_wallet_address: The wallet derived from private_key needs MATIC
    pub async fn set_approval_for_all_clob(&self) -> Result<()> {
        // Get addresses from SDK's contract_config
        // Based on SDK example: https://github.com/Polymarket/rs-clob-client/blob/main/examples/approvals.rs
        // - config.conditional_tokens = CTF contract (where we call setApprovalForAll)
        // - config.exchange = CTF Exchange (the operator we approve)
        let config = contract_config(POLYGON, false)
            .ok_or_else(|| anyhow::anyhow!("Failed to get contract config from SDK"))?;
        
        let ctf_contract_address = config.conditional_tokens;
        let exchange_address = config.exchange;
        
        eprintln!("🔐 Setting approval for all tokens using CTF contract's setApprovalForAll()");
        eprintln!("   CTF Contract (conditional_tokens): {:#x}", ctf_contract_address);
        eprintln!("   CTF Exchange (exchange/operator): {:#x}", exchange_address);
        eprintln!("   This will approve the Exchange contract to manage ALL your conditional tokens");
        
        // For proxy wallets, use relayer (gasless transactions)
        // For EOA wallets, use direct RPC call
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            eprintln!("   🔄 Using Polymarket relayer for proxy wallet (gasless transaction)");
            eprintln!("   Proxy wallet: {}", proxy_addr);
            
            // Use relayer to execute setApprovalForAll from proxy wallet
            // Based on: https://docs.polymarket.com/developers/builders/relayer-client
            self.set_approval_for_all_via_relayer(ctf_contract_address, exchange_address).await
        } else {
            eprintln!("   🔄 Using direct RPC call for EOA wallet");
            
            // Check if we have a private key (required for signing)
            let private_key = self.private_key.as_ref()
                .ok_or_else(|| anyhow::anyhow!("Private key is required for token approval. Please set private_key in config.json"))?;
            
            // Create signer from private key
            let signer = LocalSigner::from_str(private_key)
                .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
                .with_chain_id(Some(POLYGON));
            
            let signer_address = signer.address();
            eprintln!("   💰 Wallet that needs MATIC for gas: {:#x}", signer_address);
            
            // Use direct RPC call like SDK example (instead of relayer)
            // Based on: https://github.com/Polymarket/rs-clob-client/blob/main/examples/approvals.rs
            const RPC_URL: &str = "https://polygon-rpc.com";
            
            let provider = ProviderBuilder::new()
                .wallet(signer.clone())
                .connect(RPC_URL)
                .await
                .context("Failed to connect to Polygon RPC")?;
            
            // Create IERC1155 contract instance
            let ctf = IERC1155::new(ctf_contract_address, provider.clone());
            
            eprintln!("   📤 Sending setApprovalForAll transaction via direct RPC call...");
            
            // Call setApprovalForAll directly (like SDK example)
            let tx_hash = ctf
                .setApprovalForAll(exchange_address, true)
                .send()
                .await
                .context("Failed to send setApprovalForAll transaction")?
                .watch()
                .await
                .context("Failed to watch setApprovalForAll transaction")?;
            
            eprintln!("   ✅ Successfully sent setApprovalForAll transaction!");
            eprintln!("   Transaction Hash: {:#x}", tx_hash);
            
            Ok(())
        }
    }
    
    /// Set approval for all tokens via Polymarket relayer (for proxy wallets)
    /// Based on: https://docs.polymarket.com/developers/builders/relayer-client
    /// 
    /// NOTE: For signature_type 2 (GNOSIS_SAFE), the relayer expects a complex Safe transaction format
    /// with nonce, Safe address derivation, struct hash signing, etc. This implementation uses a
    /// simpler format that may work for signature_type 1 (POLY_PROXY). If you get 400/401 errors
    /// with signature_type 2, the full Safe transaction flow needs to be implemented.
    async fn set_approval_for_all_via_relayer(
        &self,
        ctf_contract_address: Address,
        exchange_address: Address,
    ) -> Result<()> {
        // Check signature_type - warn if using GNOSIS_SAFE (type 2) as it may need different format
        if let Some(2) = self.signature_type {
            eprintln!("   ⚠️  Using signature_type 2 (GNOSIS_SAFE) - relayer may require Safe transaction format");
            eprintln!("   💡 If this fails, the full Safe transaction flow (nonce, Safe address, struct hash) may be needed");
        }
        
        // Function signature: setApprovalForAll(address operator, bool approved)
        // Function selector: keccak256("setApprovalForAll(address,bool)")[0:4] = 0xa22cb465
        let function_selector = hex::decode("a22cb465")
            .context("Failed to decode function selector")?;
        
        // Encode parameters: (address operator, bool approved)
        let mut encoded_params = Vec::new();
        
        // Encode operator address (20 bytes, left-padded to 32 bytes)
        let mut operator_bytes = [0u8; 32];
        operator_bytes[12..].copy_from_slice(exchange_address.as_slice());
        encoded_params.extend_from_slice(&operator_bytes);
        
        // Encode approved (bool) - true = 1, padded to 32 bytes
        let approved_bytes = U256::from(1u64).to_be_bytes::<32>();
        encoded_params.extend_from_slice(&approved_bytes);
        
        // Combine function selector with encoded parameters
        let mut call_data = function_selector;
        call_data.extend_from_slice(&encoded_params);
        
        let call_data_hex = format!("0x{}", hex::encode(&call_data));
        
        eprintln!("   📝 Encoded call data: {}", call_data_hex);
        
        // Use relayer for gasless transaction. The /execute path returns 404; the
        // builder-relayer-client uses POST /submit. See: Polymarket/builder-relayer-client
        const RELAYER_SUBMIT: &str = "https://relayer-v2.polymarket.com/submit";
        
        eprintln!("   📤 Sending setApprovalForAll transaction via relayer (POST /submit)...");
        
        // Build transaction for relayer (matches SafeTransaction: to, operation=Call, data, value)
        let ctf_address_str = format!("{:#x}", ctf_contract_address);
        let transaction = serde_json::json!({
            "to": ctf_address_str,
            "operation": 0u8,   // 0 = Call
            "data": call_data_hex,
            "value": "0"
        });
        
        let relayer_request = serde_json::json!({
            "transactions": [transaction],
            "description": format!("Set approval for all tokens - approve Exchange contract {:#x}", exchange_address)
        });
        
        // Add authentication headers (Builder API credentials)
        let api_key = self.api_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API key required for relayer. Please set api_key in config.json"))?;
        let api_secret = self.api_secret.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API secret required for relayer. Please set api_secret in config.json"))?;
        let api_passphrase = self.api_passphrase.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API passphrase required for relayer. Please set api_passphrase in config.json"))?;
        
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let timestamp_str = timestamp_ms.to_string();
        
        let body_string = serde_json::to_string(&relayer_request)
            .context("Failed to serialize relayer request")?;
        
        let signature = Self::builder_relayer_signature(
            api_secret,
            timestamp_ms,
            "POST",
            "/submit",
            &body_string,
        )?;
        
        // Send request to relayer
        let response = self.client
            .post(RELAYER_SUBMIT)
            .header("User-Agent", "polymarket-trading-bot/1.0")
            .header("POLY_BUILDER_API_KEY", api_key)
            .header("POLY_BUILDER_TIMESTAMP", &timestamp_str)
            .header("POLY_BUILDER_PASSPHRASE", api_passphrase)
            .header("POLY_BUILDER_SIGNATURE", &signature)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&relayer_request)
            .send()
            .await
            .context("Failed to send setApprovalForAll request to relayer")?;
        
        let status = response.status();
        let response_text = response.text().await
            .context("Failed to read relayer response")?;
        
        if !status.is_success() {
            let sig_type_hint = if self.signature_type == Some(2) {
                "\n\n   💡 For signature_type 2 (GNOSIS_SAFE), the relayer expects a Safe transaction format:\n\
                  - Get nonce from /nonce endpoint\n\
                  - Derive Safe address from signer\n\
                  - Build SafeTx struct hash\n\
                  - Sign and pack signature\n\
                  - Send: { from, to, proxyWallet, data, nonce, signature, signatureParams, type: \"SAFE\", metadata }\n\
                  \n\
                  Consider using signature_type 1 (POLY_PROXY) if possible, or implement the full Safe flow."
            } else {
                ""
            };
            
            anyhow::bail!(
                "Relayer rejected setApprovalForAll request (status: {}): {}\n\
                \n\
                CTF Contract Address: {:#x}\n\
                Exchange Contract Address: {:#x}\n\
                Signature Type: {:?}\n\
                \n\
                This may be a relayer endpoint issue, authentication problem, or request format mismatch.\n\
                Please verify your Builder API credentials are correct.{}",
                status, response_text, ctf_contract_address, exchange_address, self.signature_type, sig_type_hint
            );
        }
        
        // Parse relayer response
        let relayer_response: serde_json::Value = serde_json::from_str(&response_text)
            .context("Failed to parse relayer response")?;
        
        let transaction_id = relayer_response["transactionID"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing transactionID in relayer response"))?;
        
        eprintln!("   ✅ Successfully sent setApprovalForAll transaction via relayer!");
        eprintln!("   Transaction ID: {}", transaction_id);
        eprintln!("   💡 The relayer will execute this transaction from your proxy wallet (gasless)");
        
        // Wait for transaction confirmation (like TypeScript SDK's response.wait())
        eprintln!("   ⏳ Waiting for transaction confirmation...");
        self.wait_for_relayer_transaction(transaction_id).await?;
        
        Ok(())
    }

    /// Submit a signed Safe redeem transaction via Polymarket relayer (API).
    /// Uses POST /submit with type=SAFE. On success returns RedeemResponse with transaction hash.
    /// See: https://docs.polymarket.com/api-reference/relayer/submit-a-transaction
    async fn submit_safe_redeem_via_relayer(
        &self,
        from_address: AlloyAddress,
        safe_address_str: &str,
        exec_calldata: &[u8],
        nonce: U256,
        safe_sig_bytes: &[u8],
        safe_tx_gas: u64,
    ) -> Result<crate::models::RedeemResponse> {
        const RELAYER_SUBMIT: &str = "https://relayer-v2.polymarket.com/submit";
        let api_key = self.api_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API key required for relayer"))?;
        let api_secret = self.api_secret.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API secret required for relayer"))?;
        let api_passphrase = self.api_passphrase.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API passphrase required for relayer"))?;

        // Relayer expects lowercase addresses and string values for all fields (per API docs)
        let to_lower = |s: &str| s.strip_prefix("0x").map(|h| format!("0x{}", h.to_lowercase())).unwrap_or_else(|| s.to_lowercase());
        let from_hex = format!("0x{}", hex::encode(from_address.as_slice()));
        let to_hex = to_lower(safe_address_str);
        let signature_params = serde_json::json!({
            "gasPrice": "0",
            "operation": "0",
            "safeTxnGas": safe_tx_gas.to_string(),
            "baseGas": "0",
            "gasToken": "0x0000000000000000000000000000000000000000",
            "refundReceiver": "0x0000000000000000000000000000000000000000"
        });
        let relayer_body = serde_json::json!({
            "from": from_hex.to_lowercase(),
            "to": to_hex,
            "proxyWallet": to_hex,
            "data": format!("0x{}", hex::encode(exec_calldata)),
            "nonce": nonce.to_string(),
            "signature": format!("0x{}", hex::encode(safe_sig_bytes)),
            "signatureParams": signature_params,
            "type": "SAFE"
        });
        let body_string = serde_json::to_string(&relayer_body)
            .context("Serialize relayer body")?;
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let signature = Self::builder_relayer_signature(
            api_secret,
            timestamp_ms,
            "POST",
            "/submit",
            &body_string,
        )?;

        eprintln!("   📤 Submitting Safe redeem via Polymarket relayer (API)...");
        let response = self.client
            .post(RELAYER_SUBMIT)
            .header("User-Agent", "polymarket-trading-bot/1.0")
            .header("POLY_BUILDER_API_KEY", api_key)
            .header("POLY_BUILDER_TIMESTAMP", timestamp_ms.to_string())
            .header("POLY_BUILDER_PASSPHRASE", api_passphrase)
            .header("POLY_BUILDER_SIGNATURE", &signature)
            .header("Content-Type", "application/json")
            .json(&relayer_body)
            .send()
            .await
            .context("Relayer submit request")?;

        let status = response.status();
        let response_text = response.text().await.context("Relayer response body")?;
        if !status.is_success() {
            anyhow::bail!("Relayer rejected redeem (status {}): {}", status, response_text);
        }
        let parsed: serde_json::Value = serde_json::from_str(&response_text).context("Parse relayer JSON")?;
        let transaction_id = parsed["transactionID"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing transactionID in relayer response"))?;
        eprintln!("   ✅ Relayer accepted transaction ID: {}", transaction_id);
        let tx_hash = self.wait_for_relayer_transaction(transaction_id).await?;
        Ok(crate::models::RedeemResponse {
            success: true,
            message: Some(format!("Redeem submitted via Polymarket relayer. Tx: {}", tx_hash)),
            transaction_hash: Some(tx_hash),
            amount_redeemed: None,
        })
    }

    /// Wait for relayer transaction to be confirmed (like TypeScript SDK's response.wait())
    /// Polls the relayer status endpoint until transaction reaches STATE_CONFIRMED or STATE_FAILED
    async fn wait_for_relayer_transaction(&self, transaction_id: &str) -> Result<String> {
        // Based on TypeScript SDK pattern: response.wait() returns transactionHash
        // Relayer states: STATE_NEW, STATE_EXECUTED, STATE_MINE, STATE_CONFIRMED, STATE_FAILED, STATE_INVALID
        let status_url = format!("https://relayer-v2.polymarket.com/transaction/{}", transaction_id);
        
        // Poll for transaction confirmation (with timeout)
        let max_wait_seconds = 120;
        let check_interval_seconds = 2;
        let start_time = std::time::Instant::now();
        
        loop {
            let elapsed = start_time.elapsed().as_secs();
            if elapsed >= max_wait_seconds {
                eprintln!("   ⏱️  Timeout waiting for relayer confirmation ({}s)", max_wait_seconds);
                eprintln!("   💡 Transaction was submitted but confirmation timed out");
                eprintln!("   💡 Check status at: {}", status_url);
                anyhow::bail!("Relayer transaction confirmation timeout after {} seconds", max_wait_seconds);
            }
            
            // Check transaction status
            match self.client
                .get(&status_url)
                .header("User-Agent", "polymarket-trading-bot/1.0")
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() {
                        let status_text = response.text().await
                            .context("Failed to read relayer status response")?;
                        
                        let status_data: serde_json::Value = serde_json::from_str(&status_text)
                            .context("Failed to parse relayer status response")?;
                        
                        let state = status_data["state"].as_str()
                            .unwrap_or("UNKNOWN");
                        
                        match state {
                            "STATE_CONFIRMED" => {
                                let tx_hash = status_data["transactionHash"].as_str()
                                    .unwrap_or("N/A");
                                eprintln!("   ✅ Transaction confirmed! Hash: {}", tx_hash);
                                return Ok(tx_hash.to_string());
                            }
                            "STATE_FAILED" | "STATE_INVALID" => {
                                let error_msg = status_data["metadata"].as_str()
                                    .unwrap_or("Transaction failed");
                                anyhow::bail!("Relayer transaction failed: {}", error_msg);
                            }
                            "STATE_NEW" | "STATE_EXECUTED" | "STATE_MINE" => {
                                eprintln!("   ⏳ Transaction state: {} (elapsed: {}s)", state, elapsed);
                                tokio::time::sleep(tokio::time::Duration::from_secs(check_interval_seconds)).await;
                                continue;
                            }
                            _ => {
                                eprintln!("   ⏳ Transaction state: {} (elapsed: {}s)", state, elapsed);
                                tokio::time::sleep(tokio::time::Duration::from_secs(check_interval_seconds)).await;
                                continue;
                            }
                        }
                    } else {
                        warn!("Failed to check relayer status (status: {}): will retry", response.status());
                        tokio::time::sleep(tokio::time::Duration::from_secs(check_interval_seconds)).await;
                        continue;
                    }
                }
                Err(e) => {
                    warn!("Failed to check relayer status: {} - will retry", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(check_interval_seconds)).await;
                    continue;
                }
            }
        }
    }

    /// Fallback: Approve individual tokens (ETH Up/Down, BTC Up/Down) with large allowance
    /// This is used when setApprovalForAll fails via relayer
    /// Triggers SDK auto-approval by placing tiny test sell orders for each token
    pub async fn approve_individual_tokens(&self, eth_market_data: &crate::models::Market, btc_market_data: &crate::models::Market) -> Result<()> {
        eprintln!("🔄 Fallback: Approving individual tokens with large allowance...");
        
        // Get token IDs from current markets
        let eth_condition_id = &eth_market_data.condition_id;
        let btc_condition_id = &btc_market_data.condition_id;
        
        let mut token_ids = Vec::new();
        
        // Get ETH market tokens
        if let Ok(eth_details) = self.get_market(eth_condition_id).await {
            for token in &eth_details.tokens {
                token_ids.push((token.token_id.clone(), format!("ETH {}", token.outcome)));
            }
        }
        
        // Get BTC market tokens
        if let Ok(btc_details) = self.get_market(btc_condition_id).await {
            for token in &btc_details.tokens {
                token_ids.push((token.token_id.clone(), format!("BTC {}", token.outcome)));
            }
        }
        
        if token_ids.is_empty() {
            anyhow::bail!("Could not find any token IDs from current markets");
        }
        
        eprintln!("   Found {} tokens to approve", token_ids.len());
        
        // For each token, trigger SDK auto-approval by placing a tiny test sell order
        // The SDK will automatically approve with a large amount (typically max uint256)
        let mut success_count = 0;
        let mut fail_count = 0;
        
        for (token_id, description) in &token_ids {
            eprintln!("   🔐 Checking {} token balance...", description);
            
            // Check if user has balance for this token before attempting approval
            match self.check_balance_allowance(token_id).await {
                Ok((balance, _)) => {
                    let balance_decimal = balance / rust_decimal::Decimal::from(1_000_000u64);
                    let balance_f64 = f64::try_from(balance_decimal).unwrap_or(0.0);
                    
                    if balance_f64 == 0.0 {
                        eprintln!("   ⏭️  Skipping {} token - no balance (balance: 0)", description);
                        continue; // Skip tokens user doesn't own
                    }
                    
                    eprintln!("   ✅ {} token has balance: {:.6} - triggering approval...", description, balance_f64);
                }
                Err(e) => {
                    eprintln!("   ⚠️  Could not check balance for {} token: {} - skipping", description, e);
                    continue; // Skip if we can't check balance
                }
            }
            
            // Place a tiny sell order (0.01 shares) to trigger SDK's auto-approval
            // This order will likely fail due to size, but it will trigger the approval process
            // Using 0.01 (minimum non-zero with 2 decimal places) instead of 0.000001 which rounds to 0.00
            match self.place_market_order(token_id, 0.01, "SELL", Some("FAK")).await {
                Ok(_) => {
                    eprintln!("   ✅ {} token approved successfully", description);
                    success_count += 1;
                }
                Err(e) => {
                    // Check if it's an allowance error (which means approval was triggered)
                    let error_str = format!("{}", e);
                    if error_str.contains("balance") || error_str.contains("allowance") {
                        eprintln!("   ✅ {} token approval triggered (order failed but approval succeeded)", description);
                        success_count += 1;
                    } else {
                        eprintln!("   ⚠️  {} token approval failed: {}", description, error_str);
                        fail_count += 1;
                    }
                }
            }
            
            // Small delay between approvals
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
        
        if success_count > 0 {
            eprintln!("✅ Successfully approved {}/{} tokens with large allowance", success_count, token_ids.len());
            if fail_count > 0 {
                eprintln!("   ⚠️  {} tokens failed to approve (will retry on sell if needed)", fail_count);
            }
            Ok(())
        } else {
            anyhow::bail!("Failed to approve any tokens. All {} attempts failed.", token_ids.len())
        }
    }

    /// Place a market order (FOK/FAK) for immediate execution
    /// 
    /// This is used for emergency selling or when you want immediate execution at market price.
    /// Equivalent to JavaScript: client.createAndPostMarketOrder(userMarketOrder)
    /// 
    /// Market orders execute immediately at the best available price:
    /// - FOK (Fill-or-Kill): Order must fill completely or be cancelled
    /// - FAK (Fill-and-Kill): Order fills as much as possible, remainder is cancelled
    pub async fn place_market_order(
        &self,
        token_id: &str,
        amount: f64,
        side: &str,
        order_type: Option<&str>, // "FOK" or "FAK", defaults to FOK
    ) -> Result<OrderResponse> {
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for market order")?;
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key required for order signing"))?;
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key")?
            .with_chain_id(Some(POLYGON));
        
        // Convert order side string to SDK Side enum
        let side_enum = match side {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", side),
        };
        
        // Convert order type (defaults to FOK for immediate execution)
        let order_type_enum = match order_type.unwrap_or("FOK") {
            "FOK" => OrderType::FOK,
            "FAK" => OrderType::FAK,
            _ => OrderType::FOK, // Default to FOK
        };
        
        use rust_decimal::{Decimal, RoundingStrategy};
        use rust_decimal::prelude::*;
        
        // Convert amount to Decimal
        // For BUY orders: round to 2 decimal places (USD requirement)
        // For SELL orders: round to 6 decimal places (reasonable precision for shares)
        let amount_decimal = if matches!(side_enum, Side::Buy) {
            // BUY: USD value - round to 2 decimal places (Polymarket requirement for USD)
            Decimal::from_f64_retain(amount)
            .ok_or_else(|| anyhow::anyhow!("Failed to convert amount to Decimal"))?
                .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
        } else {
            // SELL: Shares - round to 2 decimal places (Amount::shares requires <= 2 decimal places)
            // Format to 2 decimal places and parse as Decimal
            let shares_str = format!("{:.2}", amount);
            Decimal::from_str(&shares_str)
                .context(format!("Failed to parse shares '{}' as Decimal", shares_str))?
        };
        
        // For BUY orders, check USDC balance and allowance before placing order
        if matches!(side_enum, Side::Buy) {
            match self.check_usdc_balance_allowance().await {
                Ok((usdc_balance, usdc_allowance)) => {
                    let usdc_balance_f64 = f64::try_from(usdc_balance / rust_decimal::Decimal::from(1_000_000u64)).unwrap_or(0.0);
                    let usdc_allowance_f64 = f64::try_from(usdc_allowance / rust_decimal::Decimal::from(1_000_000u64)).unwrap_or(0.0);
                    let order_f64 = f64::try_from(amount_decimal).unwrap_or(0.0);
                    eprintln!("   USDC: ${:.2} balance, ${:.2} needed", usdc_balance_f64, order_f64);
                    if usdc_balance_f64 < order_f64 {
                        anyhow::bail!(
                            "Insufficient USDC balance for BUY order.\n\
                            Required: ${:.2}, Available: ${:.2}\n\
                            Please deposit USDC to your proxy wallet: {}",
                            amount_decimal, usdc_balance_f64,
                            self.proxy_wallet_address.as_deref().unwrap_or("your wallet")
                        );
                    }
                    if usdc_allowance_f64 < order_f64 {
                        eprintln!("   ⚠️  Allowance ${:.2} < order (SDK may auto-approve)", usdc_allowance_f64);
                    }
                }
                Err(e) => {
                    eprintln!("   ⚠️  USDC check failed: {} (continuing)", e);
                }
            }
        }
        
        // Use actual market order (not limit order)
        // Market orders don't require a price - they execute at the best available market price
        // The SDK handles the price automatically based on current market conditions
        // 
        // IMPORTANT: For market orders:
        // - BUY: Use USD value (Amount::usdc) - amount is USD to spend
        // - SELL: Use shares (Amount::shares) - amount is number of shares to sell
        let amount = if matches!(side_enum, Side::Buy) {
            // BUY: amount is USD value to spend
            Amount::usdc(amount_decimal)
                .context("Failed to create Amount from USD value")?
        } else {
            // SELL: amount is number of shares to sell (actual shares, not base units)
            // Ensure the Decimal is positive and non-zero
            if amount_decimal <= Decimal::ZERO {
                anyhow::bail!("Invalid shares amount: {}. Must be greater than 0.", amount_decimal);
            }
            
            // Debug: Log the exact Decimal value being passed
            eprintln!("   🔍 Creating Amount::shares with Decimal: {} (from f64: {})", amount_decimal, amount);
            eprintln!("   🔍 Decimal scale: {} (Amount::shares requires <= 2)", amount_decimal.scale());
            
            // Amount::shares() requires Decimal with <= 2 decimal places
            // Round to 2 decimal places if needed
            let rounded_shares = if amount_decimal.scale() > 2 {
                let rounded = amount_decimal.round_dp_with_strategy(2, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
                eprintln!("   🔄 Rounded from {} to {} (scale: {})", amount_decimal, rounded, rounded.scale());
                rounded
            } else {
                amount_decimal
            };
            
            // Ensure the Decimal is positive and non-zero
            if rounded_shares <= Decimal::ZERO {
                anyhow::bail!("Invalid shares amount: {}. Must be greater than 0.", rounded_shares);
            }
            
            Amount::shares(rounded_shares)
                .context(format!("Failed to create Amount from shares: {}. Ensure the value is valid and has <= 2 decimal places.", rounded_shares))?
        };
        
        let order_builder = client
            .market_order()
            .token_id(token_id)
            .amount(amount)
            .side(side_enum)
            .order_type(order_type_enum);
        
        // Post order and capture detailed error information
        // For SELL orders, the SDK should handle token approval automatically on the first attempt
        // However, if it fails with allowance error, retry with increasing delays to allow SDK to approve
        // Each conditional token (BTC/ETH) is a separate ERC-20 contract and needs its own approval
        // For SELL orders, try posting with retries for allowance errors
        let mut retry_count = 0;
        let max_retries = if matches!(side_enum, Side::Sell) { 3 } else { 1 }; // Increased to 3 retries for SELL orders
        
        let response = loop {
            // Rebuild order builder for each retry (since it's moved when building)
            let order_builder_retry = client
                .market_order()
                .token_id(token_id)
                .amount(amount.clone())
                .side(side_enum)
                .order_type(order_type_enum);
            
            // Build and sign the order (rebuild for each retry since SignedOrder doesn't implement Clone)
            let order_to_sign = order_builder_retry.build().await?;
            let signed_order = client.sign(&signer, order_to_sign)
                .await
                .context("Failed to sign market order")?;
            
            let result = client.post_order(signed_order).await;
            
            match result {
                Ok(resp) => {
                    // Success - break out of retry loop
                    break resp;
                }
                Err(e) => {
                    let error_str = format!("{:?}", e);
                    // Separate balance errors from allowance errors
                    // Balance error: You don't own enough tokens (shouldn't retry)
                    // Allowance error: You own tokens but haven't approved contract (should retry - SDK may auto-approve)
                    let is_allowance_error = error_str.contains("allowance") || 
                                           (error_str.contains("not enough") && error_str.contains("allowance"));
                    let is_balance_error = error_str.contains("balance") && !error_str.contains("allowance");
                    
                    retry_count += 1;
                    
                    // Single concise log line (avoid verbose repetition; caller will log context)
                    let short_msg = if error_str.contains("no orders found to match") || error_str.contains("FAK order") {
                        "FAK: no liquidity to match"
                    } else if error_str.contains("FOK") || error_str.contains("fully filled") {
                        "FOK: could not fill"
                    } else if is_balance_error {
                        "Insufficient balance"
                    } else if is_allowance_error {
                        "Allowance issue"
                    } else {
                        error_str.lines().next().unwrap_or("Order rejected").trim()
                    };
                    let token_short = if token_id.len() > 16 { format!("{}…", &token_id[..16]) } else { token_id.to_string() };
                    eprintln!("❌ Order failed: {} | {} ${} token:{}", short_msg, side, amount_decimal, token_short);
                    
                    // Only retry for allowance errors on SELL orders (not balance errors)
                    // Balance errors mean you don't have the tokens - retrying won't help
                    // Allowance errors mean SDK may need time to auto-approve - retrying can help
                    // CRITICAL: Refresh backend's cached allowance before retrying
                    // The backend checks cached allowance, not on-chain approval directly
                    if is_allowance_error && matches!(side_enum, Side::Sell) && retry_count < max_retries {
                        eprintln!("   ⚠️  Allowance error detected - refreshing backend cache before retry...");
                        eprintln!("   💡 Backend checks cached allowance, not on-chain approval directly");
                        if let Err(refresh_err) = self.update_balance_allowance_for_sell(token_id).await {
                            eprintln!("   ⚠️  Failed to refresh allowance cache: {} (retrying anyway)", refresh_err);
                        } else {
                            eprintln!("   ✅ Allowance cache refreshed - waiting 500ms for backend to process...");
                            // Give backend a moment to process the cache update
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        }
                        // All retries wait 0.5s for selling
                        let wait_millis = 500;
                        eprintln!("   🔄 Waiting {}ms before retry (attempt {}/{})...", wait_millis, retry_count, max_retries);
                        tokio::time::sleep(tokio::time::Duration::from_millis(wait_millis)).await;
                        continue; // Retry the order
                    }
                    
                    // For balance errors, don't retry - return error immediately
                    if is_balance_error {
                        anyhow::bail!(
                            "Insufficient token balance: {}\n\
                            Order details: Side={}, Amount={}, Token ID={}\n\
                            \n\
                            This is a portfolio balance issue - you don't own enough tokens.\n\
                            Retrying won't help. Please check your Polymarket portfolio.",
                            error_str, side, amount_decimal, token_id
                        );
                    }
                    
                    // DISABLED: If we've exhausted retries, try setApprovalForAll before giving up
                    // Temporarily disabled - approval functions are disabled throughout the codebase
                    // if is_allowance_error && matches!(side_enum, Side::Sell) && retry_count >= max_retries {
                    //     eprintln!("\n⚠️  Token allowance issue detected after {} attempts", retry_count);
                    //     eprintln!("   Attempting to approve all tokens using setApprovalForAll()...");
                    //     
                    //     // Try to approve all tokens at once using setApprovalForAll
                    //     if let Err(approval_err) = self.set_approval_for_all_clob().await {
                    //         eprintln!("   ⚠️  Failed to set approval for all tokens: {}", approval_err);
                    //         eprintln!("   💡 Each conditional token (BTC/ETH) needs separate approval - SDK may have approved ETH but not BTC");
                    //         eprintln!("   This order will be retried on the next check cycle.");
                    //         eprintln!("   If it continues to fail, you may need to manually approve this token on Polymarket UI.");
                    //     } else {
                    //         eprintln!("   ✅ Successfully approved all tokens via setApprovalForAll()");
                    //         eprintln!("   💡 Retrying sell order after approval...");
                    //         // Wait a moment for approval to propagate
                    //         tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    //         // Retry the order one more time after approval
                    //         continue;
                    //     }
                    // }
                    
                    // Return the error - the bot will retry on the next check cycle
                    if is_allowance_error {
                        if matches!(side_enum, Side::Buy) {
                            // For BUY orders, this is USDC allowance issue
                            anyhow::bail!(
                                "Insufficient USDC allowance for BUY order: {}\n\
                                Order details: Side=BUY, Amount=${}, Token ID={}\n\
                                \n\
                                USDC allowance issue - SDK may need more time to auto-approve USDC.\n\
                                \n\
                                To fix:\n\
                                1. Check USDC approval: cargo run --bin test_allowance -- --check\n\
                                2. Approve USDC manually via Polymarket UI if needed\n\
                                3. Or wait for SDK to auto-approve (will retry on next cycle)\n\
                                \n\
                                This order will be retried on the next check cycle.",
                                error_str, amount_decimal, token_id
                            );
                        } else {
                            // For SELL orders, this is conditional token allowance issue
                            anyhow::bail!(
                                "Insufficient allowance: {}\n\
                                Order details: Side=SELL, Amount={}, Token ID={}\n\
                                \n\
                                Token allowance issue - SDK may need more time to auto-approve.\n\
                                This order will be retried on the next check cycle.",
                                error_str, amount_decimal, token_id
                            );
                        }
                    }
                    
                    anyhow::bail!(
                        "Failed to post market order: {}\n\
                        Order details: Side={}, Amount={}, Token ID={}",
                        e, side, amount_decimal, token_id
                    );
                }
            }
        };
        
        // Check if the response indicates failure even if the request succeeded
        if !response.success {
            let error_msg = response.error_msg.as_deref().unwrap_or("Unknown error");
            eprintln!("❌ Order rejected by API: {}", error_msg);
            eprintln!("   Order details:");
            eprintln!("      Token ID: {}", token_id);
            eprintln!("      Side: {}", side);
            eprintln!("      Amount: ${}", amount_decimal);
            eprintln!("      Type: Market order (price determined by market)");
                anyhow::bail!(
                    "Order was rejected: {}\n\
                    Order details: Token ID={}, Side={}, Amount=${}",
                    error_msg, token_id, side, amount_decimal
                );
        }
        
        // Convert SDK response to our OrderResponse format
        let order_response = OrderResponse {
            order_id: Some(response.order_id.clone()),
            status: response.status.to_string(),
            message: if response.success {
                Some(format!("Market order executed successfully. Order ID: {}", response.order_id))
            } else {
                response.error_msg.clone()
            },
        };
        
            eprintln!("   ✅ Posted | Order {}", response.order_id);
        
        Ok(order_response)
    }

    /// Place multiple market orders in a single API request (batch).
    /// Returns one response per order in the same order. Each item may be Ok or Err depending on that order's success.
    /// Confirmation time is one round-trip for the whole batch.
    pub async fn place_market_orders(
        &self,
        orders: &[(&str, f64, &str, Option<&str>)], // (token_id, amount, side, order_type)
    ) -> Result<Vec<OrderResponse>> {
        if orders.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.get_or_create_clob_client().await
            .context("Failed to get CLOB client for place_market_orders")?;
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing."))?;
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key")?
            .with_chain_id(Some(POLYGON));
        use rust_decimal::Decimal;
        use rust_decimal::prelude::*;
        use rust_decimal::RoundingStrategy;
        let mut signed_orders = Vec::with_capacity(orders.len());
        for (token_id, amount, side, order_type) in orders.iter() {
            let side_enum = match *side {
                "BUY" => Side::Buy,
                "SELL" => Side::Sell,
                _ => anyhow::bail!("Invalid order side: {}", side),
            };
            let order_type_enum = match order_type.unwrap_or("FOK") {
                "FOK" => OrderType::FOK,
                "FAK" => OrderType::FAK,
                _ => OrderType::FOK,
            };
            let amount_decimal = if matches!(side_enum, Side::Buy) {
                Decimal::from_f64_retain(*amount)
                    .ok_or_else(|| anyhow::anyhow!("Failed to convert amount to Decimal"))?
                    .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
            } else {
                let shares_str = format!("{:.2}", amount);
                Decimal::from_str(&shares_str).context("Parse shares")?
            };
            let amount_typed = if matches!(side_enum, Side::Buy) {
                Amount::usdc(amount_decimal).context("Amount::usdc")?
            } else {
                Amount::shares(amount_decimal).context("Amount::shares")?
            };
            let order_to_sign = client
                .market_order()
                .token_id(*token_id)
                .amount(amount_typed)
                .side(side_enum)
                .order_type(order_type_enum)
                .build()
                .await?;
            let signed = client.sign(&signer, order_to_sign).await?;
            signed_orders.push(signed);
        }
        let responses = client.post_orders(signed_orders).await
            .context("Failed to post batch orders")?;
        let mut out = Vec::with_capacity(responses.len());
        for (i, resp) in responses.into_iter().enumerate() {
            let token_id = orders.get(i).map(|o| o.0).unwrap_or("?");
            out.push(OrderResponse {
                order_id: Some(resp.order_id.clone()),
                status: resp.status.to_string(),
                message: if resp.success {
                    Some(format!("Order ID: {}", resp.order_id))
                } else {
                    resp.error_msg.clone()
                },
            });
            if !resp.success {
                eprintln!("   Order {} (token {}...) rejected: {}", i + 1, &token_id[..token_id.len().min(16)], resp.error_msg.as_deref().unwrap_or("unknown"));
            }
        }
        Ok(out)
    }
    
    /// Place an order using REST API with HMAC authentication (fallback method)
    /// 
    /// NOTE: This is a fallback method. The main place_order() method uses the official SDK
    /// with proper private key signing. Use this only if SDK integration fails.
    #[allow(dead_code)]
    async fn place_order_hmac(&self, order: &OrderRequest) -> Result<OrderResponse> {
        let path = "/orders";
        let url = format!("{}{}", self.clob_url, path);
        
        // Serialize order to JSON string for signature
        let body = serde_json::to_string(order)
            .context("Failed to serialize order to JSON")?;
        
        let mut request = self.client.post(&url).json(order);
        
        // Add HMAC-SHA256 authentication headers (L2 authentication)
        request = self.add_auth_headers(request, "POST", path, &body)
            .context("Failed to add authentication headers")?;

        eprintln!("📤 Posting order to Polymarket (HMAC): {} {} {} @ {}", 
              order.side, order.size, order.token_id, order.price);

        let response = request
            .send()
            .await
            .context("Failed to place order")?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            
            // Provide helpful error messages
            if status == 401 || status == 403 {
                anyhow::bail!(
                    "Authentication failed (status: {}): {}",
                    status, error_text
                );
            }
            
            anyhow::bail!("Failed to place order (status: {}): {}", status, error_text);
        }

        let order_response: OrderResponse = response
            .json()
            .await
            .context("Failed to parse order response")?;

        eprintln!("✅ Order placed successfully: {:?}", order_response);
        Ok(order_response)
    }

    /// True if API credentials (api_key, api_secret, api_passphrase) are set. Required for CLOB-authenticated calls (e.g. balance check, portfolio scan fallback).
    pub fn has_api_credentials(&self) -> bool {
        self.api_key.is_some() && self.api_secret.is_some() && self.api_passphrase.is_some()
    }

    /// Return the wallet address to use for positions/redemption: proxy_wallet_address if set, else EOA from private_key.
    pub fn get_wallet_address(&self) -> Result<String> {
        if let Some(ref proxy) = self.proxy_wallet_address {
            return Ok(proxy.clone());
        }
        let pk = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("private_key required to get wallet address"))?;
        let signer = LocalSigner::from_str(pk)
            .context("Invalid private_key")?;
        Ok(format!("{}", signer.address()))
    }

    /// Fetch redeemable position condition IDs from Data API (user=wallet, redeemable=true).
    /// Only includes positions where the wallet holds tokens (size > 0).
    pub async fn get_redeemable_positions(&self, wallet: &str) -> Result<Vec<String>> {
        let url = "https://data-api.polymarket.com/positions";
        let user = if wallet.starts_with("0x") {
            wallet.to_string()
        } else {
            format!("0x{}", wallet)
        };
        let response = self.client
            .get(url)
            .query(&[("user", user.as_str()), ("redeemable", "true"), ("limit", "500")])
            .send()
            .await
            .context("Failed to fetch redeemable positions")?;
        if !response.status().is_success() {
            anyhow::bail!("Data API returned {} for redeemable positions", response.status());
        }
        let positions: Vec<Value> = response.json().await.unwrap_or_default();
        let mut condition_ids: Vec<String> = positions
            .iter()
            .filter(|p| {
                let size = p.get("size")
                    .and_then(|s| s.as_f64())
                    .or_else(|| p.get("size").and_then(|s| s.as_u64().map(|u| u as f64)))
                    .or_else(|| p.get("size").and_then(|s| s.as_str()).and_then(|s| s.parse::<f64>().ok()));
                size.map(|s| s > 0.0).unwrap_or(false)
            })
            .filter_map(|p| p.get("conditionId").and_then(|c| c.as_str()).map(|s| {
                if s.starts_with("0x") { s.to_string() } else { format!("0x{}", s) }
            }))
            .collect();
        condition_ids.sort();
        condition_ids.dedup();
        Ok(condition_ids)
    }

    /// Redeem winning conditional tokens after market resolution
    /// 
    /// This uses the CTF (Conditional Token Framework) contract to redeem winning tokens
    /// Derive the Gnosis Safe (proxy wallet) address for Polygon from the EOA signer.
    /// Matches TypeScript deriveSafe: getCreate2Address(factory, salt, initCodeHash).
    /// Constants from builder-relayer-client: SafeFactory, SAFE_INIT_CODE_HASH.
    fn derive_safe_address_polygon(eoa: &AlloyAddress) -> AlloyAddress {
        const SAFE_FACTORY_POLYGON: [u8; 20] = [
            0xaa, 0xcf, 0xee, 0xa0, 0x3e, 0xb1, 0x56, 0x1c, 0x4e, 0x67,
            0xd6, 0x61, 0xe4, 0x06, 0x82, 0xbd, 0x20, 0xe3, 0x54, 0x1b,
        ];
        const SAFE_INIT_CODE_HASH: [u8; 32] = [
            0x2b, 0xce, 0x21, 0x27, 0xff, 0x07, 0xfb, 0x63, 0x2d, 0x16, 0xc8, 0x34, 0x7c, 0x4e, 0xbf, 0x50,
            0x1f, 0x48, 0x41, 0x16, 0x8b, 0xed, 0x00, 0xd9, 0xe6, 0xef, 0x71, 0x5d, 0xdb, 0x6f, 0xce, 0xcf,
        ];
        // Salt = keccak256(abi.encode(address)) — 32 bytes: 12 zero + 20 byte address
        let mut salt_input = [0u8; 32];
        salt_input[12..32].copy_from_slice(eoa.as_slice());
        let salt = keccak256(salt_input);
        // CREATE2: keccak256(0xff ++ deployer (20) ++ salt (32) ++ initCodeHash (32))[12..32]
        let mut preimage = Vec::with_capacity(85);
        preimage.push(0xff);
        preimage.extend_from_slice(&SAFE_FACTORY_POLYGON);
        preimage.extend_from_slice(salt.as_slice());
        preimage.extend_from_slice(&SAFE_INIT_CODE_HASH);
        let hash = keccak256(&preimage);
        AlloyAddress::from_slice(&hash[12..32])
    }

    /// for USDC at 1:1 ratio after market resolution.
    /// 
    /// Parameters:
    /// - condition_id: The condition ID of the resolved market
    /// - token_id: The token ID of the winning token (used to determine index_set)
    /// - outcome: "Up" or "Down" to determine the index set
    /// 
    /// Reference: Polymarket CTF redemption using SDK
    /// USDC collateral address: 0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174
    /// 
    /// Note: This implementation uses the SDK's CTF client if available.
    /// The exact module path may vary - check SDK documentation.
    pub async fn redeem_tokens(
        &self,
        condition_id: &str,
        token_id: &str,
        outcome: &str,
    ) -> Result<RedeemResponse> {
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;

        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));

        let parse_address_hex = |s: &str| -> Result<AlloyAddress> {
            let hex_str = s.strip_prefix("0x").unwrap_or(s);
            let bytes = hex::decode(hex_str).context("Invalid hex in address")?;
            let len = bytes.len();
            let arr: [u8; 20] = bytes.try_into().map_err(|_| anyhow::anyhow!("Address must be 20 bytes, got {}", len))?;
            Ok(AlloyAddress::from(arr))
        };

        let collateral_token = parse_address_hex("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
            .context("Failed to parse USDC address")?;

        let condition_id_clean = condition_id.strip_prefix("0x").unwrap_or(condition_id);
        let condition_id_b256 = B256::from_str(condition_id_clean)
            .context(format!("Failed to parse condition_id as B256: {}", condition_id))?;

        let index_set = if outcome.to_uppercase().contains("UP") || outcome == "1" {
            U256::from(1)
        } else {
            U256::from(2)
        };

        eprintln!("Redeeming winning tokens for condition {} (outcome: {}, index_set: {})",
              condition_id, outcome, index_set);

        const CTF_CONTRACT: &str = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045";
        // Use alternate public RPC to avoid 401/API key disabled from polygon-rpc.com
        const RPC_URL: &str = "https://polygon-bor-rpc.publicnode.com";
        const PROXY_WALLET_FACTORY: &str = "0xaB45c5A4B0c941a2F231C04C3f49182e1A254052";

        let ctf_address = parse_address_hex(CTF_CONTRACT)
            .context("Failed to parse CTF contract address")?;

        let parent_collection_id = B256::ZERO;
        let use_proxy = self.proxy_wallet_address.is_some();
        let sig_type = self.signature_type.unwrap_or(1);
        let index_sets: Vec<U256> = vec![index_set];
        eprintln!("   Prepared redemption parameters:");
        eprintln!("   - CTF Contract: {}", ctf_address);
        eprintln!("   - Collateral token (USDC): {}", collateral_token);
        eprintln!("   - Condition ID: {} ({:?})", condition_id, condition_id_b256);
        eprintln!("   - Index set(s): {:?} (outcome: {})", index_sets, outcome);

        // Pre-check: condition must be resolved on-chain (payoutDenominator > 0)
        let provider_resolution = ProviderBuilder::new().connect(RPC_URL).await.ok();
        if let Some(prov) = &provider_resolution {
            let payout_denom_selector = keccak256("payoutDenominator(bytes32)".as_bytes()).as_slice()[..4].to_vec();
            let mut payout_calldata = Vec::with_capacity(4 + 32);
            payout_calldata.extend_from_slice(&payout_denom_selector);
            payout_calldata.extend_from_slice(condition_id_b256.as_slice());
            let payout_tx = TransactionRequest::default()
                .to(ctf_address)
                .input(Bytes::from(payout_calldata).into());
            if let Ok(res) = prov.call(payout_tx).await {
                let arr: [u8; 32] = res.as_ref().try_into().unwrap_or([0u8; 32]);
                let denom = U256::from_be_slice(&arr);
                if denom == U256::ZERO {
                    anyhow::bail!(
                        "Condition {} is not resolved on-chain yet (payoutDenominator=0). \
                        Wait for the oracle to report the outcome, then try again.",
                        &condition_id[..condition_id.len().min(18)]
                    );
                }
                eprintln!("   Condition resolved on-chain (payoutDenominator={})", denom);
            }
        }

        // Manual ABI encode for redeemPositions(address,bytes32,bytes32,uint256[])
        let selector = hex::decode("3d7d3f5a").context("redeemPositions selector")?;
        let build_redeem_calldata = |index_list: &[U256]| -> Vec<u8> {
            let mut calldata = Vec::new();
            calldata.extend_from_slice(&selector);
            let mut addr_bytes = [0u8; 32];
            addr_bytes[12..].copy_from_slice(collateral_token.as_slice());
            calldata.extend_from_slice(&addr_bytes);
            calldata.extend_from_slice(parent_collection_id.as_slice());
            calldata.extend_from_slice(condition_id_b256.as_slice());
            calldata.extend_from_slice(&U256::from(32u32 * 4).to_be_bytes::<32>());
            calldata.extend_from_slice(&U256::from(index_list.len()).to_be_bytes::<32>());
            for idx in index_list {
                calldata.extend_from_slice(&idx.to_be_bytes::<32>());
            }
            calldata
        };
        let mut redeem_calldata = build_redeem_calldata(&index_sets);

        // sig_type 2: proxy_wallet_address must be a Gnosis Safe (v1.3); it must have nonce(), getTransactionHash(), execTransaction()
        let (tx_to, tx_data, gas_limit, used_safe_redemption) = if use_proxy && sig_type == 2 {
            let safe_address_str = self.proxy_wallet_address.as_deref()
                .ok_or_else(|| anyhow::anyhow!("proxy_wallet_address required for Safe redemption"))?;
            let safe_address = parse_address_hex(safe_address_str)
                .context("Failed to parse proxy_wallet_address (Safe address)")?;
            eprintln!("   Using Gnosis Safe (sig_type 2): signing and executing redemption via Safe.execTransaction");
            let provider_read = ProviderBuilder::new()
                .connect(RPC_URL)
                .await
                .context("Failed to connect to RPC for Safe read calls")?;
            // Pre-check: Safe on-chain balance. If 0, direct RPC redeem will revert; still try relayer when API credentials exist (custody flow).
            if !token_id.is_empty() {
                let balance_of_selector = keccak256("balanceOf(address,uint256)".as_bytes()).as_slice()[..4].to_vec();
                let mut balance_calldata = Vec::with_capacity(4 + 32 + 32);
                balance_calldata.extend_from_slice(&balance_of_selector);
                let mut addr_padded = [0u8; 32];
                addr_padded[12..].copy_from_slice(safe_address.as_slice());
                balance_calldata.extend_from_slice(&addr_padded);
                let token_id_u256 = U256::from_str(token_id).unwrap_or(U256::ZERO);
                balance_calldata.extend_from_slice(&token_id_u256.to_be_bytes::<32>());
                let balance_tx = TransactionRequest::default()
                    .to(ctf_address)
                    .input(Bytes::from(balance_calldata).into());
                if let Ok(res) = provider_read.call(balance_tx).await {
                    let arr: [u8; 32] = res.as_ref().try_into().unwrap_or([0u8; 32]);
                    let balance = U256::from_be_slice(&arr);
                    if balance == U256::ZERO {
                        let has_relayer = self.api_key.is_some() && self.api_secret.is_some() && self.api_passphrase.is_some();
                        if has_relayer {
                            eprintln!("   ⚠️  Safe holds 0 of this token on-chain (Polymarket may custody it). Proceeding to try relayer (API) redeem.");
                        } else {
                            anyhow::bail!(
                                "Your Safe ({}) holds 0 balance of this conditional token on-chain. \
                                Set api_key, api_secret, and api_passphrase in config to attempt redeem via Polymarket relayer, or redeem at https://polymarket.com (Portfolio → Redeem).",
                                safe_address_str
                            );
                        }
                    } else {
                        eprintln!("   Safe holds {} units of conditional token on-chain", balance);
                    }
                }
            }
            // Simulate CTF.redeemPositions as the Safe so we get the real revert reason (Safe.execTransaction does not revert on inner failure)
            let sim_ctf = TransactionRequest::default()
                .from(safe_address)
                .to(ctf_address)
                .input(Bytes::from(redeem_calldata.clone()).into())
                .value(U256::ZERO);
            if let Err(e) = provider_read.call(sim_ctf).await {
                // For binary markets, try the other index set (Polymarket may map Up=2, Down=1)
                let index_set_alt = if index_set == U256::ONE { U256::from(2) } else { U256::ONE };
                let redeem_alt = build_redeem_calldata(&[index_set_alt]);
                let sim_alt = TransactionRequest::default()
                    .from(safe_address)
                    .to(ctf_address)
                    .input(Bytes::from(redeem_alt.clone()).into())
                    .value(U256::ZERO);
                if let Err(_) = provider_read.call(sim_alt).await {
                    // Polymarket docs: redeem with [1, 2] for binary (both outcomes; only winning pays)
                    let redeem_both = build_redeem_calldata(&[U256::ONE, U256::from(2)]);
                    let sim_both = TransactionRequest::default()
                        .from(safe_address)
                        .to(ctf_address)
                        .input(Bytes::from(redeem_both.clone()).into())
                        .value(U256::ZERO);
                    if let Err(_) = provider_read.call(sim_both).await {
                        anyhow::bail!(
                            "CTF redeem would revert (simulated as Safe): {}. \
                            Tried index sets [{}], [{}], and [1,2]; all reverted. The CTF may not hold collateral for this market — redeem at https://polymarket.com (Portfolio → Redeem).",
                            e, index_set, index_set_alt
                        );
                    }
                    redeem_calldata = redeem_both;
                    eprintln!("   Using index sets [1, 2] (single-index simulations reverted)");
                } else {
                    redeem_calldata = redeem_alt;
                    eprintln!("   Using index set {} (simulation with {} reverted)", index_set_alt, index_set);
                }
            }
            let nonce_selector = keccak256("nonce()".as_bytes());
            let nonce_calldata: Vec<u8> = nonce_selector.as_slice()[..4].to_vec();
            let nonce_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(nonce_calldata.clone()).into());
            let nonce_result = match provider_read.call(nonce_tx).await {
                Ok(r) => r,
                Err(e) => {
                    anyhow::bail!(
                        "Safe.nonce() failed: {}. \
                        For sig_type 2, proxy_wallet_address must be a Gnosis Safe (has nonce()). \
                        If you use Polymarket proxy (Magic Link/Google), use signature_type: 1 in config.",
                        e
                    );
                }
            };
            let nonce_bytes: [u8; 32] = nonce_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("Safe.nonce() did not return 32 bytes"))?;
            let nonce = U256::from_be_slice(&nonce_bytes);
            const SAFE_TX_GAS: u64 = 300_000;
            let get_tx_hash_sig = "getTransactionHash(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,uint256)";
            let get_tx_hash_selector = keccak256(get_tx_hash_sig.as_bytes()).as_slice()[..4].to_vec();
            let zero_addr = [0u8; 32];
            let mut to_enc = [0u8; 32];
            to_enc[12..].copy_from_slice(ctf_address.as_slice());
            let data_offset_get_hash = U256::from(32u32 * 10u32);
            let mut get_tx_hash_calldata = Vec::new();
            get_tx_hash_calldata.extend_from_slice(&get_tx_hash_selector);
            get_tx_hash_calldata.extend_from_slice(&to_enc);
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&data_offset_get_hash.to_be_bytes::<32>());
            get_tx_hash_calldata.push(0); get_tx_hash_calldata.extend_from_slice(&[0u8; 31]);
            get_tx_hash_calldata.extend_from_slice(&U256::from(SAFE_TX_GAS).to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&zero_addr);
            get_tx_hash_calldata.extend_from_slice(&zero_addr);
            get_tx_hash_calldata.extend_from_slice(&nonce.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::from(redeem_calldata.len()).to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&redeem_calldata);
            let get_tx_hash_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(get_tx_hash_calldata).into());
            let tx_hash_result = provider_read.call(get_tx_hash_tx).await
                .context("Failed to call Safe.getTransactionHash()")?;
            let tx_hash_to_sign: B256 = tx_hash_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("getTransactionHash did not return 32 bytes"))?;
            const EIP191_PREFIX: &[u8] = b"\x19Ethereum Signed Message:\n32";
            let mut eip191_message = Vec::with_capacity(EIP191_PREFIX.len() + 32);
            eip191_message.extend_from_slice(EIP191_PREFIX);
            eip191_message.extend_from_slice(tx_hash_to_sign.as_slice());
            let hash_to_sign = keccak256(&eip191_message);
            let sig = signer.sign_hash(&hash_to_sign).await
                .context("Failed to sign Safe transaction hash")?;
            let sig_bytes = sig.as_bytes();
            let r = &sig_bytes[0..32];
            let s = &sig_bytes[32..64];
            let v = sig_bytes[64];
            let v_safe = if v == 27 || v == 28 { v + 4 } else { v };
            let mut packed_sig: Vec<u8> = Vec::with_capacity(85);
            packed_sig.extend_from_slice(r);
            packed_sig.extend_from_slice(s);
            packed_sig.extend_from_slice(&[v_safe]);
            let get_threshold_selector = keccak256("getThreshold()".as_bytes()).as_slice()[..4].to_vec();
            let threshold_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(get_threshold_selector).into());
            let threshold_result = provider_read.call(threshold_tx).await
                .context("Failed to call Safe.getThreshold()")?;
            let threshold_bytes: [u8; 32] = threshold_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("getThreshold did not return 32 bytes"))?;
            let threshold = U256::from_be_slice(&threshold_bytes);
            if threshold > U256::from(1) {
                let owner = signer.address();
                let mut with_owner = Vec::with_capacity(20 + packed_sig.len());
                with_owner.extend_from_slice(owner.as_slice());
                with_owner.extend_from_slice(&packed_sig);
                packed_sig = with_owner;
            }
            let safe_sig_bytes = packed_sig;
            let exec_sig = "execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,bytes)";
            let exec_selector = keccak256(exec_sig.as_bytes()).as_slice()[..4].to_vec();
            let data_offset = 32u32 * 10u32;
            let sigs_offset = data_offset + 32 + redeem_calldata.len() as u32;
            let mut exec_calldata = Vec::new();
            exec_calldata.extend_from_slice(&exec_selector);
            exec_calldata.extend_from_slice(&to_enc);
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::from(data_offset).to_be_bytes::<32>());
            exec_calldata.push(0); exec_calldata.extend_from_slice(&[0u8; 31]);
            exec_calldata.extend_from_slice(&U256::from(SAFE_TX_GAS).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&zero_addr);
            exec_calldata.extend_from_slice(&zero_addr);
            exec_calldata.extend_from_slice(&U256::from(sigs_offset).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::from(redeem_calldata.len()).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&redeem_calldata);
            exec_calldata.extend_from_slice(&U256::from(safe_sig_bytes.len()).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&safe_sig_bytes);
            // Try Polymarket relayer first (API) when credentials are set; fall back to RPC on failure
            if self.api_key.is_some() && self.api_secret.is_some() && self.api_passphrase.is_some() {
                match self.submit_safe_redeem_via_relayer(
                    signer.address(),
                    safe_address_str,
                    &exec_calldata,
                    nonce,
                    &safe_sig_bytes,
                    SAFE_TX_GAS,
                ).await {
                    Ok(resp) => {
                        eprintln!("Successfully redeemed via Polymarket relayer (API).");
                        return Ok(resp);
                    }
                    Err(e) => {
                        eprintln!("   ⚠️  Relayer redeem failed: {}. Falling back to RPC...", e);
                    }
                }
            }
            (safe_address, exec_calldata, 400_000u64, true)
        } else if use_proxy && sig_type == 1 {
            eprintln!("   Using proxy wallet: sending redemption via Proxy Wallet Factory");
            let factory_address = parse_address_hex(PROXY_WALLET_FACTORY)
                .context("Failed to parse Proxy Wallet Factory address")?;
            let selector = keccak256("proxy((uint8,address,uint256,bytes)[])".as_bytes());
            let proxy_selector = &selector.as_slice()[..4];
            let mut proxy_calldata = Vec::with_capacity(4 + 32 * 3 + 128 + 32 + redeem_calldata.len());
            proxy_calldata.extend_from_slice(proxy_selector);
            proxy_calldata.extend_from_slice(&U256::from(32u32).to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&U256::from(1u32).to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&U256::from(96u32).to_be_bytes::<32>());
            let mut type_code = [0u8; 32];
            type_code[31] = 1;
            proxy_calldata.extend_from_slice(&type_code);
            let mut to_bytes = [0u8; 32];
            to_bytes[12..].copy_from_slice(ctf_address.as_slice());
            proxy_calldata.extend_from_slice(&to_bytes);
            proxy_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&U256::from(128u32).to_be_bytes::<32>());
            let data_len = redeem_calldata.len();
            proxy_calldata.extend_from_slice(&U256::from(data_len).to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&redeem_calldata);
            (factory_address, proxy_calldata, 400_000u64, false)
        } else {
            eprintln!("   Sending redemption from EOA to CTF contract");
            (ctf_address, redeem_calldata, 300_000, false)
        };

        let provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect(RPC_URL)
            .await
            .context("Failed to connect to Polygon RPC")?;

        // Optional: simulate Safe redeem before sending to surface CTF revert reason
        if used_safe_redemption {
            let sim_tx = TransactionRequest::default()
                .from(signer.address())
                .to(tx_to)
                .input(Bytes::from(tx_data.clone()).into())
                .value(U256::ZERO)
                .gas_limit(gas_limit);
            if let Err(e) = provider.call(sim_tx).await {
                eprintln!("   ⚠️  Pre-flight simulation failed (inner redeem would revert): {}", e);
                anyhow::bail!(
                    "Redeem would revert. Simulation: {}. \
                    Check conditionId, indexSet (Up=1, Down=2), and that the Safe holds the token. \
                    If the market uses a non-zero parentCollectionId, redeem via Polymarket app.",
                    e
                );
            }
        }

        let tx_request = TransactionRequest::default()
            .to(tx_to)
            .input(Bytes::from(tx_data).into())
            .value(U256::ZERO)
            .gas_limit(gas_limit)
            .max_fee_per_gas(200_000_000_000u128)
            .max_priority_fee_per_gas(30_000_000_000u128);

        let pending_tx = match provider.send_transaction(tx_request).await {
            Ok(tx) => tx,
            Err(e) => {
                let err_msg = format!("Failed to send redeem transaction: {}", e);
                eprintln!("   {}", err_msg);
                anyhow::bail!("{}", err_msg);
            }
        };

        let tx_hash = *pending_tx.tx_hash();
        eprintln!("   Transaction sent, waiting for confirmation...");
        eprintln!("   Transaction hash: {:?}", tx_hash);

        let receipt = pending_tx.get_receipt().await
            .context("Failed to get transaction receipt")?;

        if !receipt.status() {
            anyhow::bail!("Redemption transaction failed. Transaction hash: {:?}", tx_hash);
        }

        if used_safe_redemption {
            let payout_redemption_topic = keccak256(
                b"PayoutRedemption(address,address,bytes32,bytes32,uint256[],uint256)"
            );
            let logs = receipt.logs();
            let ctf_has_payout = logs.iter().any(|log| {
                log.address() == ctf_address && log.topics().first().map(|t| t.as_slice()) == Some(payout_redemption_topic.as_slice())
            });
            if !ctf_has_payout {
                anyhow::bail!(
                    "Redemption tx was mined but the inner redeem reverted (no PayoutRedemption from CTF). \
                    Possible causes: (1) Condition not yet resolved on-chain (oracle may not have reported); (2) tokens already redeemed; (3) conditionId/outcome/indexSet mismatch; (4) wrong collateral or parentCollectionId. \
                    If the market just resolved, wait a few minutes and retry. Check the tx on Polygonscan for revert reason. Tx: {:?}",
                    tx_hash
                );
            }
        }

        let redeem_response = RedeemResponse {
            success: true,
            message: Some(format!("Successfully redeemed tokens. Transaction: {:?}", tx_hash)),
            transaction_hash: Some(format!("{:?}", tx_hash)),
            amount_redeemed: None,
        };
        eprintln!("Successfully redeemed winning tokens!");
        eprintln!("Transaction hash: {:?}", tx_hash);
        if let Some(block_number) = receipt.block_number {
            eprintln!("Block number: {}", block_number);
        }
        Ok(redeem_response)
    }

    /// Merge complete sets of Up and Down tokens for a condition into USDC.
    /// Burns min(Up_balance, Down_balance) pairs and returns that much USDC via the CTF relayer.
    /// Uses the same redeemPositions(conditionId, [1,2]) flow as redeem_tokens.
    pub async fn merge_complete_sets(&self, condition_id: &str) -> Result<RedeemResponse> {
        self.redeem_tokens(condition_id, "", "Up+Down (merge complete sets)").await
    }
}

