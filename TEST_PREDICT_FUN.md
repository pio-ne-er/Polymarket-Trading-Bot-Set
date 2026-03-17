# Predict.fun API Test Script

This test script helps you test the Predict.fun API to:
1. Fetch market prices (bid/ask)
2. Test order creation (buy/sell)

## Setup

1. **Get API Key** (for mainnet):
   - Join Predict's Discord: https://discord.gg/predictdotfun
   - Open a support ticket to request an API key

2. **Choose Environment**:
   - **Testnet**: `https://api-testnet.predict.fun` (no API key required)
   - **Mainnet**: `https://api.predict.fun` (API key required)

## Running the Test Script

```bash
cargo run --bin test_predict_fun
```

## Current Status

The script tests the following endpoints (all use `/v1/` prefix):
- ✅ Get Markets (`GET /v1/markets`)
- ✅ Get Market by ID (`GET /v1/markets/{id}`)
- ✅ Get Orderbook (`GET /v1/markets/{id}/orderbook`)
- ✅ Get Auth Message (`GET /v1/auth/message`)
- ⏭️ Create Order is documented; run with auth when ready (`POST /v1/orders`)

The test uses the first market from the list for orderbook and market-by-ID checks.

## Next Steps

1. **Verify API Endpoints**:
   - Check the official API docs: https://dev.predict.fun/
   - Verify the exact endpoint paths
   - Update the script if endpoints differ

2. **Get a Real Market ID**:
   - Use the markets endpoint to get active market IDs
   - Replace `test_market_id` in the script with a real ID

3. **Test Authentication**:
   - Get auth message from the API
   - Sign it with your private key (using EIP-712 or similar)
   - Use the signature to get JWT token
   - Use JWT token for authenticated requests

4. **Test Order Creation**:
   - Once authenticated, test creating buy/sell orders
   - Start with small test orders on testnet

## API Endpoints (Correct Paths)

All endpoints use the **`/v1/`** prefix. Base URLs: mainnet `https://api.predict.fun`, testnet `https://api-testnet.predict.fun`.

### Markets
- `GET /v1/markets` - Get all markets (response: `{ success, data: Market[] }`)
- `GET /v1/markets/{id}` - Get market by ID (numeric id)
- `GET /v1/markets/{id}/orderbook` - Get orderbook (bid/ask prices)

### Orders
- `POST /v1/orders` - Create an order
- `GET /v1/orders` - Get your orders
- `POST /v1/orders/remove` - Cancel orders

### Authentication
- `GET /v1/auth/message` - Get message to sign (response: `{ data: { message } }`)
- `POST /v1/auth` - Get JWT with signature (body: `{ signer, message, signature }`, response: `{ data: { token } }`)

## Example Usage

Once you have the correct endpoints and authentication:

```rust
// 1. Get auth message
let message = client.get_auth_message().await?;

// 2. Sign message with your private key (implement signing logic)
let signature = sign_message(&message, &private_key)?;
let signer_address = "0x..."; // your wallet address

// 3. Authenticate (POST /v1/auth with signer, message, signature)
client.authenticate(signer_address.to_string(), message, signature).await?;

// 4. Get markets
let markets = client.get_markets().await?;

// 5. Get orderbook for a market
let orderbook = client.get_orderbook(&markets[0].id).await?;
println!("Bids: {:?}", orderbook["bids"]);
println!("Asks: {:?}", orderbook["asks"]);

// 6. Create a test order
let order = client.create_order(
    &markets[0].id,
    "Yes",      // outcome
    "buy",      // side
    "0.50",     // price
    "1.0",      // size
    "limit"     // order type
).await?;
```

## Troubleshooting

- **404 Errors**: Check if the API endpoint structure matches the documentation
- **Authentication Errors**: Verify your signature format matches Predict.fun's requirements
- **Rate Limits**: Testnet allows 240 requests/minute, mainnet requires API key

## Resources

- API Documentation: https://dev.predict.fun/
- Alternative API Docs UI: https://api.predict.fun/docs
- Discord: https://discord.gg/predictdotfun
- TypeScript SDK: https://www.npmjs.com/package/@predictdotfun/sdk
- Python SDK: https://pypi.org/project/predict-sdk/
