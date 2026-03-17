# Polymarket Trading Bot

A Rust-based trading bot for Polymarket that monitors ETH, BTC, and Solana 15-minute price prediction markets and executes trades using momentum-based strategies.

## Contact

For help or questions, reach out on Telegram: [Pio-ne-er](https://t.me/hi_3333)

## Bot Versions

### 1. Market Order Bot (Default)
**Binary:** `polymarket-arbitrage-bot` (default)

Uses market orders (FOK - Fill-or-Kill) to buy tokens when price conditions are met.

**Strategy:**
- Buys tokens when price reaches `trigger_price` after `min_elapsed_minutes`
- Uses market orders for immediate execution
- Sells when price reaches `sell_price` or stop-loss triggers

**Run:**
```bash
# Simulation mode
cargo run -- --simulation

# Production mode
cargo run -- --no-simulation
```

### 2. Limit Order Bot
**Binary:** `polymarket-arbitrage-bot-limit`

Uses limit orders for more precise price control.

**Strategy:**
- At `min_elapsed_minutes`, places limit buy orders for both Up and Down tokens
- When a buy order fills, immediately places TWO limit sell orders for the same token:
  - One at `sell_price` (profit target)
  - One at `stop_loss_price` (stop-loss protection)
- Whichever price is hit first will execute
- Ignores `max_buy_price` and `min_time_remaining_seconds`

**Run:**
```bash
# Simulation mode
cargo run --bin polymarket-arbitrage-bot-limit -- --simulation

# Production mode
cargo run --bin polymarket-arbitrage-bot-limit -- --no-simulation
```

### 3. Price Monitor (Price Recording Only)
**Binary:** `price_monitor`

**Price monitoring and recording only - NO TRADING**

This version only monitors real-time prices and records them to history files. Perfect for data collection and analysis without any trading activity.

**Features:**
- Monitors BTC, ETH, Solana, and XRP markets
- Records prices to `history/market_<PERIOD>_prices.toml` files
- Automatically discovers new markets when 15-minute periods change
- Uses the same config.json settings for market discovery
- No authentication required (read-only price monitoring)

**Run:**
```bash
cargo run --bin price_monitor -- --config config.json
```

**Output:**
- Prices are recorded to: `history/market_<PERIOD>_prices.toml`
- Format: `[TIMESTAMP] 📊 BTC: U$bid/$ask D$bid/$ask | ETH: U$bid/$ask D$bid/$ask | SOL: U$bid/$ask D$bid/$ask | XRP: U$bid/$ask D$bid/$ask | ⏱️  TIME_REMAINING`

**Note:** This version uses the updated file naming format (period only, no condition ID) to avoid duplicate files.

### 4. Dual Limit-Start Bot (0.45)
**Binary:** `main_dual_limit_045`

Places limit buy orders for BTC and any enabled ETH/SOL/XRP markets at the start of each 15-minute market.

**Strategy:**
- At market start (first ~2 seconds of the period), place limit buys for BTC and enabled ETH/SOL/XRP Up/Down at $0.45
- Number of shares uses `trading.dual_limit_shares` if set; otherwise `fixed_trade_amount / dual_limit_price`
- No position handling after placement: when a limit order fills, it logs confirmation only (no sell orders)
- Hedge (stop-loss via opposite token): if only one side (Up/Down) fills, then after `trading.dual_limit_hedge_after_minutes` (default 10) the bot watches the unfilled token’s BUY price; when it reaches `trading.dual_limit_hedge_price` (default $0.85), it cancels the unfilled $0.45 order and places a new buy for the same shares at $0.85
- Polling interval fixed at 1s for this bot to reduce API load
- Market enable flags: `trading.enable_eth_trading`, `trading.enable_solana_trading`, `trading.enable_xrp_trading`

**Run:**
```bash
# Simulation mode
cargo run --bin main_dual_limit_045 -- --simulation

# Production mode
cargo run --bin main_dual_limit_045 -- --no-simulation

cargo run --bin backtest -- --backtest
```

### 4b. Dual Limit Same-Size Bot (0.45)
**Binary:** `main_dual_limit_045_same_size`

Same as the Dual Limit-Start Bot (0.45) but with simpler hedge behavior: if both initial orders fill, no further trading; if only one fills, **2-min / 4-min / early / standard** hedge buys the unfilled token at market for the same size and cancels the unfilled $0.45 limit. No reentry logic.

**Strategy:**
- At market start (first ~5 seconds), place limit buys for BTC and enabled ETH/SOL/XRP Up/Down at $0.45
- If **both** orders fill → no further trading for that market
- If **only one** fills: **2-min** (trailing on ask), **4-min**, **early**, or **standard** hedge — buy unfilled at market for same size (1×), cancel the unfilled limit. No immediate limit sell; position is held until the **low-price exit** below (if it applies) or market closure.

**When do we place the two limit sell orders (0.05 / 0.99 or 0.02 / 0.99)?**  
The bot places **two** limit sell orders (cheap token at $0.05 or $0.02, opposite at $0.99) only when **all** of the following are true:

1. **At least 10 minutes** have elapsed in the period.
2. The market was hedged via **4-min, early, or standard** hedge (not via 2-min). **2-min hedges do not get these exit orders** — they are held until market closure.
3. One side’s **bid** has dropped **below 0.10** (or below 0.03 for the 0.02/0.99 path when hedge price &lt; 0.60).  
   Then: sell the cheap token at **$0.05** (or $0.02) and the opposite at **$0.99**.

**Why you might not see limit sell orders:**
- You hedged via **2-min** → no low-price exit; no 0.05/0.99 orders are placed for that market.
- Less than **10 minutes** have elapsed in the period.
- No side has dropped below the threshold (e.g. bid &lt; 0.10) yet.
- (Dual-filled-at-0.45 exit is disabled by default; that would place 0.02/0.99 when both filled at 0.45 and one side &lt; 0.03.)

**Run:**
```bash
cargo run --bin main_dual_limit_045_same_size -- --simulation
cargo run --bin main_dual_limit_045_same_size -- --no-simulation
```

### 4c. Dual Limit 5-Minute BTC Bot
**Binary:** `main_dual_limit_045_5m_btc`

Dual limit at $0.45 for **BTC 5-minute markets only**. No ETH/SOL/XRP 5m markets. Two windows with **trailing + bands**: **2-min** (2–3 min) and **3-min** (≥3 min). No uptick or [0.43, 0.47] band.

**Strategy:**
- **Market start (first ~5 s):** Place limit buys for **BTC Up** and **BTC Down** at $0.45 (`dual_limit_shares` or `fixed_trade_amount / price`). Skip if already have a position for that token in this period.
- **Both fill** → no further trading.
- **Only one fills:** Track **lowest unfilled ASK** per market. Time-based **bands**: 2–3 min → ask &lt; 0.45; ≥3 min → ask &lt; 0.50. If ask ≥ band, update `lowest_ask = ask` (or **allow buy above band** if we had a valid dip: `lowest_ask ≤ band − dual_limit_hedge_trailing_stop`). **Trigger:** buy when `ask ≥ lowest_ask + dual_limit_hedge_trailing_stop` (default 0.03). Entering 3-min window resets `lowest_ask` once (new baseline) unless allowing buy above band.
  - **2-min window** (elapsed **[2 min, 3 min)**): Band 0.45, trailing on ask → buy at ask, cancel limit, record in `two_min_hedge_markets`.
  - **3-min window** (elapsed **≥ 3 min**): Band 0.50; only consider when **0.55 &lt; ask &lt; hedge_price** (default 0.85). Same trailing; on trigger → buy at ask, cancel limit, record in `hedge_executed_for_market`.
- **Early placement:** After 3 min, if at least one side is filled, try to discover the **next** 5m period and place limit buys for it before it starts.

**Constants (in code):** `NINETY_SEC_AFTER_SECONDS = 120`, `THREE_MIN_AFTER_SECONDS = 180`, `BAND_2MIN = 0.45`, `BAND_3MIN = 0.5`. From 3 min: no lower bound on ask (buy when price drops). Config: `dual_limit_hedge_trailing_stop` (default 0.03).

**Config:** Same `config.json` / `config-red.json` (`dual_limit_price`, `dual_limit_shares`, `dual_limit_hedge_price`, `dual_limit_hedge_trailing_stop`). ETH/SOL/XRP flags ignored (BTC 5m only).

**Run:**
```bash
# Simulation
cargo run --bin main_dual_limit_045_5m_btc -- --config config.json --simulation

# Production
cargo run --bin main_dual_limit_045_5m_btc -- --config config.json --no-simulation
cargo run --bin main_dual_limit_045_5m_btc -- --config config-red.json --no-simulation
```

**Note:** Market discovery uses slug `btc-updown-5m-<timestamp>` (5-minute period timestamp). If Polymarket uses a different slug format for 5m markets, discovery may need to be updated.

### 5. Dual Limit-Start Bot (1-hour)
**Binary:** `main_dual_limit_1h`

Same strategy as the 15-minute bot, but targets 1-hour BTC/ETH/SOL/XRP up/down markets.

**Strategy:**
- At market start (first ~2 seconds of the hour), place limit buys for BTC, ETH, SOL, and XRP Up/Down at `trading.dual_limit_price` (default $0.45)
- Number of shares uses `trading.dual_limit_shares` if set; otherwise `fixed_trade_amount / dual_limit_price`
- No position handling after placement: when a limit order fills, it logs confirmation only (no sell orders)
- Polling interval fixed at 1s for this bot to reduce API load

**Run:**
```bash
# Simulation mode
cargo run --bin main_dual_limit_1h -- --simulation

# Production mode
cargo run --bin main_dual_limit_1h -- --no-simulation
```

### 6. Trailing Bot
**Binary:** `main_trailing`

Uses the **CLOB market WebSocket** (`wss://ws-subscriptions-clob.polymarket.com/ws/market`) for real-time bid/ask prices instead of REST polling. Token IDs are subscribed on connect and when the period changes; the monitor builds snapshots from the WebSocket price cache. Logging and strategy logic are unchanged.

Waits until one token’s price is **under 0.45**, then starts trailing that token. Applies a **trailing stop** for the first buy (with a 0.45 trigger cap and reset-if-above-0.45 rule), then **stop loss + trailing stop** for the opposite token.

**Strategy:**
- **Entry:** Do **not** start trailing from market start. Wait until one token (Up or Down) has price **&lt; 0.45**. Then start trailing that token (track lowest/highest). If both are under 0.45, pick the one with lower ask.
- **First token (trailing stop):** No time-window price bands. Track **lowest** and **highest** of the chosen token. When **current ≥ lowest + `trailing_stop_point`**: if **trigger &gt; 0.45** ignore and set lowest = 0.45; if price goes **above 0.45** without triggering, **reset** and wait for under 0.45 again. When trigger is valid (trigger ≤ 0.45) and current is not above the recorded highest, **buy** (min cost $1). Remember **shares** and **price** bought.
- **Second token (opposite):** All time windows are **from the moment the first token was bought** (not market start). Hedging windows depend on market period:
  - **15-minute markets (defaults):** first window 2 min, second 4 min. **5-minute markets (defaults):** first window 1 min, second 2m30s. Override with `trailing_first_window_seconds` and `trailing_second_window_seconds` (e.g. 90 and 180 for 1m30s and 3min).
  - **Stop loss (always active):** Buy opposite when opposite price **≥ (1 − first_bought_price + 0.10)**. No ceiling check; the buy always executes. (Buffer 0.10 = `STOP_LOSS_BUFFER` in code.)
  - **Trailing stop:** Buy when **current ≥ opposite_lowest + `trailing_stop_point`** only if opposite price is at or below the ceiling for **time since first buy**:
    - **Within first window of first buy** (2 min for 15m, 1 min for 5m): opposite price ≤ (1 − first_bought − 0.05)
    - **Within second window** (4 min for 15m, 2m30s for 5m): opposite price ≤ (1 − first_bought)
    - **After early-hedge minutes from first buy** (config `dual_limit_early_hedge_minutes`): opposite price ≤ (1 − first_bought)
  - Between second window and early-hedge from first buy, ceiling remains (1 − first_bought). If the trailing trigger fires but opposite is above the ceiling, the buy is skipped.
  - **After second window (2m30s / 4min):** If **first_bought_price + opposite_ask > 1.1**, buy the second token **immediately** (no trailing or ceiling check). If the sum is ≤ 1.1, keep trailing the second token as above.
- **One hedge per market:** After the second buy, the market is marked done.
- **Second buy top-up:** If the second (opposite) token buy fills for less than the first token amount (e.g. partial fill or balance check failed), the trader spawns a background task that waits ~5s, checks balance, and places a market top-up order for the shortfall so the opposite side matches the first buy size (same logic as dual-limit hedge top-up).
- **Hedge balance reconciliation:** After the second buy (live only), the bot waits 5s, then fetches both token balances. If the difference is **greater than 1 share**, it places one market order to buy more of the **lower** side so that the two sides end up within **1 share** of each other (critical for equal exposure at resolution).
- **Balance checking with retries:** All balance checks that affect trade size (post-buy confirmation, hedge top-up, reconciliation) use **retries** (e.g. 5 attempts, 4s apart) so that delayed chain/indexer updates don’t produce 0 or wrong balances. The reported balance is the **maximum** seen across attempts to avoid understating the position.
- **Most exact fill size: order `size_matched`:** After a market buy, the bot first tries to get the filled amount from the **exchange order** (CLOB API get_order → `size_matched`). That is the most exact source: the exchange reports how much the order filled as soon as it matches, with no chain or indexer delay. If that fails, it falls back to balance checks with retries, then to the requested order size.
- **Cached CLOB connection:** The authenticated CLOB client (TCP + TLS + L2 auth) is created once and reused for all order and balance calls. This avoids a new handshake on every `place_market_order`, `get_order_filled_shares`, `check_balance_only`, `cancel_order`, etc., reducing latency for sending and confirming orders. Call `api.clear_clob_client_cache()` after a 401 if you need to force re-auth.
- **History:** Each completed pair is appended to **`history/trailing_trades.jsonl`** with `up_bought_price`, `up_shares`, `down_bought_price`, `down_shares`, and `mode` (simulation/live).

**Config (in `config.json` / `config.example.json`):**
- `trading.trailing_stop_point` – trailing step in price (e.g. 0.03). Default 0.03.
- `trading.trailing_shares` – number of shares per buy. Default 10 (or falls back to `dual_limit_shares` / `fixed_trade_amount / 0.5`).
- `trading.trailing_market_minutes` – market period: **15** for 15-minute markets (e.g. `btc-updown-15m-*`), **5** for 5-minute markets (e.g. `btc-updown-5m-*`). Default: 15. Hedging window defaults: 5m → 1 min, 2m30s; 15m → 2 min, 4 min (overridable via the window config below).
- `trading.trailing_stop_loss_enabled` – when **true** (default), the second (opposite) token is also bought when price ≥ (1 − first_bought + 0.10) (stop loss). When **false**, only the trailing-stop trigger is used for the second buy (no stop-loss buy).
- `trading.trailing_use_websocket` – when **true** (default), prices are read from the CLOB market WebSocket. When **false**, prices are fetched via REST polling (same as pre–WebSocket behavior).
- `trading.trailing_first_window_seconds` – (optional) second-token first hedging window in seconds. If set, overrides the default (60 for 5m markets, 120 for 15m). Example: **90** for 1 minute 30 seconds.
- `trading.trailing_second_window_seconds` – (optional) second-token second hedging window in seconds. If set, overrides the default (150 for 5m, 240 for 15m). Example: **180** for 3 minutes.

**Run:**
```bash
# Simulation (no real orders)
cargo run --bin main_trailing -- --simulation

# Production
cargo run --bin main_trailing -- --no-simulation
```

### 7. Backtest Mode
**Binary:** `backtest`

Simulates the Dual Limit-Start Bot (0.45) strategy using historical price data from the `history/` folder.

**How it works:**
- Reads all `history/market_*_prices.toml` files
- For each 15-minute period:
  - Assumes two limit buy orders placed at start: Up at $0.45, Down at $0.45
  - Simulates order fills when ask price <= $0.45
  - Applies hedge logic at 10 minutes (if enabled)
  - Determines winner from final prices (token with ask > 0.50 wins)
  - Calculates PnL: winning token = $1.00, losing token = $0.00
- Aggregates results across all periods

**Output:**
- Total periods tested
- Win rate (winning vs losing periods)
- Total cost, total value, total PnL
- Per-period detailed results

**Run:**
```bash
cargo run --bin backtest -- --backtest
```

**Note:** Requires price history files in `history/` folder (generated by `price_monitor` binary).

## Test Cases

### 1. Test Limit Order
**Binary:** `test_limit_order`

Test placing a limit order on Polymarket.

**Usage:**
```bash
# Use defaults (BTC Up, $0.55, 5 shares, 1 min expiration)
cargo run --bin test_limit_order

# Custom price (e.g., 60 cents)
cargo run --bin test_limit_order -- --price-cents 60

# Custom shares (e.g., 10 shares)
cargo run --bin test_limit_order -- --shares 10

# Custom expiration (e.g., 5 minutes)
cargo run --bin test_limit_order -- --expiration-minutes 5

# Specify token ID directly
cargo run --bin test_limit_order -- --token-id <TOKEN_ID>

# Custom side (BUY or SELL)
cargo run --bin test_limit_order -- --side SELL
```

**Options:**
- `-t, --token-id <TOKEN_ID>` - Token ID to buy (optional - auto-discovers BTC Up if not provided)
- `-p, --price-cents <CENTS>` - Price in cents (default: 55 = $0.55)
- `-s, --shares <SHARES>` - Number of shares (default: 5)
- `-e, --expiration-minutes <MINUTES>` - Expiration time in minutes (default: 1)
- `-c, --config <PATH>` - Config file path (default: config.json)
- `--side <SIDE>` - Order side: BUY or SELL (default: BUY)

### 2. Test Redeem
**Binary:** `test_redeem`

Redeem winning tokens from your portfolio after market resolution.

**Usage:**
```bash
# Scan portfolio and list all tokens with balance
cargo run --bin test_redeem -- --list

# Redeem all winning tokens automatically
cargo run --bin test_redeem -- --redeem-all

# Redeem a specific token
cargo run --bin test_redeem -- --token-id <TOKEN_ID>

# Just check portfolio without redeeming
cargo run --bin test_redeem -- --check-only
```

**Options:**
- `-t, --token-id <TOKEN_ID>` - Token ID to redeem (optional - scans portfolio if not provided)
- `-c, --config <PATH>` - Config file path (default: config.json)
- `--check-only` - Just check portfolio without redeeming
- `--list` - Scan portfolio and list all tokens with balance
- `--redeem-all` - Redeem all winning tokens in portfolio automatically

### 3. Test Merge (Up and Down token)
**Binary:** `test_merge`

Test the merge logic for Up and Down token amounts. A "complete set" is one Up + one Down; merging N sets corresponds to N × $1 collateral.

**Usage:**
```bash
# Default: check balance of current BTC 15-minute Up/Down and show merge result
cargo run --bin test_merge

# Run unit tests only (no API)
cargo run --bin test_merge -- --unit

# Use a specific market by condition ID
cargo run --bin test_merge -- --condition-id <CONDITION_ID> --config config.json

# Execute merge: redeem complete sets (Up+Down) to USDC via CTF relayer
cargo run --bin test_merge -- --merge
```

**Options:**
- `--unit` - Run unit tests only; no API or balance check
- `--condition-id <ID>` - Use this market instead of current BTC 15m
- `--merge` - Execute merge: submit CTF redeemPositions for complete sets (Up+Down → USDC). Requires Builder API credentials in config. No-op if complete_sets = 0.
- `-c, --config <PATH>` - Config file path (default: config.json)

**Default run:** Discovers the current (or most recent) BTC 15-minute Up/Down market, fetches your Up and Down token balances via the API, runs the merge logic, and prints: **BTC Up balance**, **BTC Down balance**, **Complete sets (mergeable)**, **Remaining Up**, **Remaining Down**. With `--merge`, also submits a relayer transaction to merge that many complete sets into USDC.

**Unit test cases:** equal amounts (5,5)→5 sets; more Up than Down (5,3)→3 sets, 2 Up left; more Down than Up (2,7)→2 sets, 5 Down left; zeros and fractional amounts.

### 4. Test Allowance
**Binary:** `test_allowance`

Check balance/allowance and manage token approvals.

**Usage:**
```bash
# Set on-chain approval (required once per proxy wallet before selling)
cargo run --bin test_allowance -- --approve-only

# Run approval, then test the cache refresh
cargo run --bin test_allowance -- --approve

# List all tokens with balance and allowance
cargo run --bin test_allowance -- --list

# Test update_balance_allowance_for_sell on a token
cargo run --bin test_allowance -- --token-id <TOKEN_ID>
```

**Options:**
- `--approve` - Run setApprovalForAll first, then the update_balance_allowance test
- `--approve-only` - Only run setApprovalForAll and exit
- `-c, --config <PATH>` - Config file path (default: config.json)
- `-t, --token-id <TOKEN_ID>` - Token ID to test (auto-picks first token with balance if not provided)
- `-i, --iterations <N>` - Number of iterations for cache-refresh test
- `-d, --delay-ms <MS>` - Delay between iterations in milliseconds
- `--list` - List all tokens with balance and allowance

**Important:** `update_balance_allowance` only **refreshes** the CLOB backend's cache from the chain. It does **not** set on-chain approval. If allowance is 0, the chain has no approval → the cache stays 0. You must run **setApprovalForAll** first.

## How It Works

## Setup

1. Install Rust (if not already installed):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. Build the project:
   ```bash
   cargo build --release
   ```

3. Configure the bot:
   - Edit `config.json` (created on first run) or use command-line arguments
   - Set `eth_condition_id` and `btc_condition_id` if you know them
   - Otherwise, the bot will attempt to discover them automatically

## Usage

### Simulation Mode (Default)
Test the bot without executing real trades:
```bash
cargo run -- --simulation
```

### Production Mode
Execute real trades (requires API key):
```bash
cargo run -- --no-simulation
```

### Configuration Options

- `--simulation` / `--no-simulation`: Toggle simulation mode
- `--config <path>`: Specify config file path (default: `config.json`)

### Configuration File

The bot creates a `config.json` file on first run with the following structure:

```json
{
  "polymarket": {
    "gamma_api_url": "https://gamma-api.polymarket.com",
    "clob_api_url": "https://clob.polymarket.com",
    "ws_url": "wss://clob-ws.polymarket.com",
    "api_key": null
  },
  "trading": {
    "min_profit_threshold": 0.01,
    "max_position_size": 100.0,
    "eth_condition_id": null,
    "btc_condition_id": null,
    "check_interval_ms": 1000
  }
}
```

**Important Settings:**
- `min_profit_threshold`: Minimum profit (in dollars) required to execute a trade
- `max_position_size`: Maximum amount to invest per trade
- `check_interval_ms`: How often to check for opportunities (in milliseconds)
- `api_key`: Your Polymarket API key (required for production mode)

## How the Bot Detects Opportunities

1. **Market Discovery**: The bot searches for active ETH and BTC 15-minute markets using Polymarket's Gamma API
2. **Price Monitoring**: Continuously fetches order book data to get current ask prices for Up/Down tokens
3. **Arbitrage Calculation**: For each combination (ETH Up + BTC Down, ETH Down + BTC Up), calculates total cost
4. **Opportunity Detection**: If total cost < $1.00 and profit >= `min_profit_threshold`, executes trade
5. **Trade Execution**: Places simultaneous buy orders for both tokens

## Testing Allowance

The `test_allowance` binary checks balance/allowance and can run **setApprovalForAll** (on-chain) and/or **update_balance_allowance** (backend cache refresh).

**Important:** `update_balance_allowance` only **refreshes** the CLOB backend’s cache from the chain. It does **not** set on-chain approval. If allowance is 0, the chain has no approval → the cache stays 0. You must run **setApprovalForAll** first.

**Set on-chain approval (required once per proxy wallet before selling):**
```bash
cargo run --bin test_allowance -- --approve-only
```

**Run approval, then test the cache refresh:**
```bash
cargo run --bin test_allowance -- --approve
```

**List all tokens with balance and allowance:**
```bash
cargo run --bin test_allowance -- --list
```

**Test `update_balance_allowance_for_sell` on a token** (only useful after `--approve-only` or `--approve` if allowance was 0):
- Auto-pick the first token with balance: `cargo run --bin test_allowance`
- Use a specific token ID: `cargo run --bin test_allowance -- --token-id <TOKEN_ID>`

**Options:**
- `--approve` — Run setApprovalForAll first, then the update_balance_allowance test
- `--approve-only` — Only run setApprovalForAll and exit
- `-c, --config <path>` — Config file (default: `config.json`)
- `-i, --iterations <N>`, `-d, --delay-ms <ms>` — For the cache-refresh test

The tool prints balance and allowance **before** and **after** the update. If allowance stays 0, it will prompt you to run with `--approve` or `--approve-only`.

## Notes

- The bot runs continuously until stopped (Ctrl+C)
- In simulation mode, all trades are logged but not executed
- The bot automatically discovers condition IDs if not provided in config
- Make sure you have sufficient balance and API permissions for production trading
