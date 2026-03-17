# Trading Logic (main_dual_limit_045_same_size)

Same-size dual-limit strategy: place **Up** and **Down** limit buys at **$0.45** at market start. If both fill → no further trading (optional dual-filled exit later). If only one fills → cancel the other limit and hedge the unfilled side via **2-min**, **4-min**, **early**, or **standard** hedge. Exits: **low-price exit** (and optional **dual-filled exit**).

---

## Config (from config.json)

| Key | Your value | Meaning |
|-----|------------|--------|
| `dual_limit_price` | 0.45 | Limit order price for Up/Down |
| `dual_limit_shares` | 6 | Shares per limit order (same size for hedge) |
| `dual_limit_hedge_after_minutes` | 10 | Minutes before standard hedge is allowed |
| `dual_limit_hedge_price` | 0.85 | Standard hedge: buy unfilled when bid ≥ this |
| `dual_limit_early_hedge_minutes` | 6 | Minutes before early hedge (0.65–0.85 band) |
| `dual_limit_hedge_trailing_stop` | 0.03 | 2-min: trigger when price ≥ lowest + this (buy at ask) |
| `trailing_stop_point` | 0.02 | Legacy / other trailing tolerance |
| `fixed_trade_amount` | 4.5 | Fallback when shares not from config |

Markets: BTC (and optionally ETH, SOL, XRP) per `enable_*_trading`.

---

## 1. Order placement (market start only)

- **When:** First **5 seconds** of a new 15-minute period (`time_elapsed_seconds ≤ 5`), once per period.
- **What:** One **limit buy at $0.45** for **Up** and one for **Down** on each enabled market (e.g. BTC). Size = `dual_limit_shares` (e.g. 6) or `fixed_trade_amount / 0.45`.
- **If both fill at 0.45:** No hedge; hold both. Optional **dual-filled low-price exit** later (see §7). No standard/early/4-min/2-min hedge.

---

## 2. On one side filled → cancel the other limit

- **When:** As soon as exactly one of the two limit orders is filled (any time in the period).
- **Action:** Cancel the **unfilled** side’s $0.45 limit order immediately (no wait). Trailing/hedge logic then decides when to buy the unfilled side.
- **No double hedge:** Once a market is in `hedge_executed_for_market` or `two_min_hedge_markets`, 4-min / early / standard hedge are skipped for that market.

---

## 3. 2-min hedge (trailing stop over lowest)

- **When:** One side filled, unfilled side’s price **&lt; $0.50** (so we’re in “cheap” territory). Runs as soon as one side fills (can trigger before 2 minutes).
- **Price band by time (unfilled token):**
  - **Before 2 min:** only consider buying when ask &lt; **0.45** (near limit).
  - **2–4 min:** ask &lt; **0.50**.
  - **After 4 min:** ask &lt; **0.65**.
- **Trigger:** Track **lowest** unfilled price (bid). When **current bid ≥ lowest + `dual_limit_hedge_trailing_stop`** (e.g. 0.03) → “bounce” → **market buy at current ASK** (same shares as the filled side).
- **Execution:** Market buy at **ask**; cancel any remaining unfilled $0.45 limit. Market is added to `two_min_hedge_markets` and `hedge_executed_for_market`; stored hedge price is that ask (used later for low-price exit path).
- **Skip:** If unfilled price is very near 0.45 (to avoid double fill with limit), unless the opposite limit was already cancelled.

---

## 4. Early high price (before 4 min) — immediate buy at 0.75

- **When:** One side filled, **before 4 min** elapsed, unfilled **ask ≥ $0.75**.
- **Action:** **Immediate market buy** at that ask (same size as filled side). Cancel unfilled $0.45 limit. Record in `hedge_executed_for_market` and store hedge price. Do not wait for 4-min block.

---

## 5. 4-min hedge (after 4 min)

- **When:** `time_elapsed_seconds ≥ 240` (4 min), one side filled, market not already in `hedge_executed_for_market` or `two_min_hedge_markets`.
- **Two cases:**
  - **Unfilled bid &gt; $0.75:** **Immediate market buy** at current **ask** (no trailing). Then cancel unfilled limit, record hedge.
  - **Unfilled bid in (0.50, 0.65):** Trailing on **bid**: track minimum bid; when **current bid ≤ min + trailing_stop** (i.e. at/near the low), **market buy at ask**. Same size; cancel limit; record hedge.
- **If unfilled drops to ≤ 0.50:** 2-min logic applies instead (cheaper band).

---

## 6. Early hedge (e.g. after 6 min)

- **When:** `time_elapsed_seconds ≥ early_hedge_after_seconds` (e.g. 6 min), one side filled, unfilled **bid in (0.65, 0.85)**. Disabled if `EARLY_HEDGE_DISABLED`.
- **Trigger:** Trailing on **bid**: track minimum bid in that band; when **current bid ≤ min + trailing_stop** → market buy at **ask**. Same size; cancel limit; record in `hedge_executed_for_market` and store hedge price.
- **If unfilled bid ≥ 0.85:** Standard hedge handles it (buy at market when time and price allow).

---

## 7. Standard hedge (e.g. after 10 min)

- **When:** `time_elapsed_seconds ≥ hedge_after_seconds` (e.g. 10 min), **exactly one** side filled, unfilled **bid ≥ `dual_limit_hedge_price`** (e.g. 0.85). Disabled if `STANDARD_HEDGE_DISABLED`.
- **Action:** **Market buy** unfilled side at current price (same number of **shares** as limit orders). Cancel unfilled $0.45 limit. Record in `hedge_executed_for_market` and store hedge price.
- **Max price:** Unfilled bid must be ≤ **$0.54** (`MAX_HEDGE_PRICE`) or we skip to avoid locking in a loss.

---

## 8. Low-price exit (after 10 min, hedged markets only)

- **When:** `time_elapsed_seconds ≥ 600` (10 min), market is in `hedge_executed_for_market` and **not** in `two_min_hedge_markets` (i.e. hedged via 4-min / early / standard, not 2-min).
- **Condition:** **Both** sides filled (limit + hedge), and **one** side’s bid **&lt; $0.10** (`LOW_PRICE_THRESHOLD`).
- **Path A — Hedge price ≥ $0.60:** Place **limit sells**: **$0.05** for the cheap token, **$0.99** for the other; same share count (capped by balances).
- **Path B — Hedge price &lt; $0.60:** When one side **&lt; $0.03** (same threshold as dual-filled exit): place **$0.02** for the cheap token, **$0.99** for the other (hedge-under-60 exit).
- **2-min hedges:** Do **not** use this low-price exit (no 0.05/0.99 or 0.02/0.99); 2-min hedges are tracked separately.

---

## 9. Dual-filled low-price exit (both filled at 0.45)

- **When:** **Both** Up and Down filled at **$0.45** (no hedge), `time_elapsed_seconds ≥ 600`, and **any** token bid **&lt; $0.03** (`DUAL_FILLED_LOW_THRESHOLD`). Only if `DUAL_FILLED_LIMIT_SELL_ENABLED` is true (currently **false** in code).
- **Action:** Place **limit sell $0.02** for the cheap token and **$0.99** for the other; same size (balance-capped).

---

## Constants (hardcoded)

| Constant | Value | Use |
|----------|--------|-----|
| `LIMIT_PRICE` | 0.45 | Dual limit order price |
| `NINETY_SEC_AFTER_SECONDS` | 120 | 2 min (name legacy) |
| `TWO_MIN_AFTER_SECONDS` | 240 | 4 min |
| `NINETY_SEC_HEDGE_MAX_PRICE` | 0.50 | 2-min band: act when unfilled &lt; 0.50 |
| `FOUR_MIN_HEDGE_MIN_PRICE` | 0.50 | 4-min band lower bound |
| `FOUR_MIN_HEDGE_MAX_PRICE` | 0.65 | 4-min band upper bound (trailing) |
| `FOUR_MIN_HEDGE_BUY_OVER_PRICE` | **0.75** | Immediate buy (before/after 4 min) when bid &gt; this |
| `EARLY_HEDGE_MIN_PRICE` | 0.65 | Early band lower |
| `EARLY_HEDGE_MAX_PRICE` | 0.85 | Early band upper |
| `LOW_PRICE_THRESHOLD` | 0.10 | Low-price exit: one side &lt; this |
| `SELL_LOW_PRICE` | 0.05 | Low-price exit (hedge ≥ 0.60): sell cheap at this |
| `SELL_HIGH_PRICE` | 0.99 | Opposite token sell price |
| `DUAL_FILLED_LOW_THRESHOLD` | 0.03 | Dual-filled / under-60 exit trigger |
| `DUAL_FILLED_SELL_LOW` | 0.02 | Cheap token sell in dual-filled / under-60 exit |
| `DUAL_FILLED_SELL_HIGH` | 0.99 | Opposite token in dual-filled / under-60 exit |
| `HEDGE_PRICE_MIN_FOR_LIMIT_SELL` | 0.60 | Above → 0.05/0.99 exit; below → 0.02/0.99 exit |
| `MAX_HEDGE_PRICE` | 0.54 | Max unfilled bid for standard hedge |
| `LIMIT_SELL_AFTER_SECONDS` | 600 | Low-price / dual-filled exits only after 10 min |

---

## Flow summary

1. **Start of period (0–5 s):** Place Up and Down limit buys at $0.45 (same size).
2. **As soon as one fills:** Cancel the other limit.
3. **Unfilled &lt; 0.50:** 2-min hedge (track lowest, buy at ask when bid ≥ lowest + 0.03).
4. **Before 4 min, unfilled ask ≥ 0.75:** Immediate market buy at ask.
5. **After 4 min:** If bid &gt; 0.75 → immediate buy at ask; else if 0.50 &lt; bid &lt; 0.65 → trailing, buy at ask.
6. **After 6 min, 0.65 &lt; bid &lt; 0.85:** Early hedge (trailing, buy at ask).
7. **After 10 min, bid ≥ 0.85:** Standard hedge (market buy, same shares).
8. **After 10 min, hedged (not 2-min):** If one side &lt; 0.10 → 0.05/0.99 (or 0.02/0.99 if hedge price &lt; 0.60).
9. **Both filled at 0.45 (optional):** If enabled and one side &lt; 0.03 → 0.02/0.99 exit.

All hedge buys are **same size** (same number of shares as the filled $0.45 order). Stored **hedge price** (ask at which we bought the unfilled side) is used only to choose the low-price exit path (0.05/0.99 vs 0.02/0.99).

---

# Trading Logic (main_dual_limit_045_5m_btc) — BTC 5-minute market

**BTC only**, **5-minute** period (300 s). Same-size dual limit at **$0.45**; hedge the unfilled side in a **2-min window** (2–3 min) or **3-min window** (≥3 min) using **trailing on ask** and bands. No uptick band, no [0.43, 0.47] near-limit band. No ETH/SOL/XRP. No low-price exit or standard “10-min” hedge — only 2-min and 3-min trailing hedge.

---

## Config (shared with 15m)

| Key | Meaning |
|-----|--------|
| `dual_limit_price` | 0.45 (limit order price) |
| `dual_limit_shares` | Shares per order (same size for hedge) |
| `dual_limit_hedge_price` | 0.85 default; from 3 min we skip when ask **≥ this** (no lower bound) |
| `dual_limit_hedge_trailing_stop` | 0.03 default; trigger when **ask ≥ lowest_ask + this** |

---

## 1. Order placement (ASAP when new market detected)

- **When:** As soon as a **new** 5-minute period/market is seen (first snapshot for that `period_timestamp`). No time window — batch limit orders are posted **immediately** when the new market is detected, not only in the first 5 seconds.
- **What:** One **limit buy at $0.45** for **BTC Up** and one for **BTC Down**. Size = `dual_limit_shares` or `fixed_trade_amount / 0.45`. Placed once per period (tracked via `last_placed_period`).
- **If both fill at 0.45:** No hedge; no further automated trading in this strategy (no low-price exit in 5m binary).

---

## 2. Cancel on one fill and start trailing stop

- **When:** As soon as **exactly one** side’s limit order is filled (any time in the period).
- **Action:** **Cancel the unfilled** side’s $0.45 limit order immediately (no wait for hedge). **Trailing stop starts right away** for the unfilled token: we track lowest ask and buy at market when **ask ≥ lowest_ask + trailing_stop** (no time gate — can trigger at 30s, 1 min, etc.).
- The unfilled limit is cancelled as soon as we detect one fill; the hedge (market buy) happens when the trailing trigger fires (any time after one fill).

---

## 3. Trailing stop (starts right after one side fills)

- **When:** **Exactly one** side filled, market not in `hedge_executed_for_market` (and, from 3 min onward, not in `two_min_hedge_markets`). **No time gate** — trailing starts on the first snapshot after one fill (e.g. 30s, 1 min, etc.). After the unfilled limit is cancelled, that trade is **removed** from pending; the code still runs trailing by using the **filled** side’s pending trade only (and the unfilled token’s ask from the snapshot).
- **Band (before 3 min elapsed):** Unfilled **ask &lt; (1 − dual_limit_price − 0.1)** (e.g. 0.45 when limit is 0.45). We track **lowest_ask**. All time windows (2/3/4 min) use **time elapsed** since period start, not remaining.
- **Above band:** If current ask **≥** band threshold, we either:
  - **Allow buy (bounce):** If **lowest_ask ≤ band − 0.03**, treat as valid bounce → allow market buy at current ask.
  - **Else:** Update lowest_ask only when current ask &lt; previous lowest; **do not** buy this snapshot.
- **Trigger:** When **current_ask ≥ lowest_ask + dual_limit_hedge_trailing_stop** (e.g. 0.03) → **market buy** at current ask (same shares as filled side), then cancel unfilled limit (if not already cancelled).
- **Band from 3 min (3–4 min only):** Band = **(1 − dual_limit_price)** (e.g. 0.55 when limit is 0.45). We only **skip** when unfilled ask **≥ hedge_price** (0.85). **No lower bound**: buying the dip is allowed. On first entry to 3-min we **reset** `lowest_ask` once (fresh baseline).
- **From 4 min:** When **time ≥ 4 min**: (1) If unfilled ask **≥ hedge_price** (0.85) → **market buy** immediately at ask. (2) If unfilled ask **&lt; 0.85** → **keep trailing** (band = 1 − limit_price and trigger **ask ≥ lowest_ask + 0.03**); when the bounce triggers we buy at market (🕐 4-MIN HEDGE (trailing)).
- **After hedge:** If hedged **before 3 min** → add to `two_min_hedge_markets` (3-min block will not run). If hedged **at or after 3 min** (including 4-min buy) → add to `hedge_executed_for_market`. Trailing state for this period is cleared.

---

## Constants (5m BTC, hardcoded)

| Constant | Value | Use |
|----------|--------|-----|
| `LIMIT_PRICE` | 0.45 | Dual limit order price |
| `PERIOD_DURATION_5M` | 300 | 5-minute period in seconds |
| `NINETY_SEC_AFTER_SECONDS` | 120 | 2-min window start (**time elapsed**; not used in current logic, 2-min = before 3 min elapsed) |
| `THREE_MIN_AFTER_SECONDS` | 180 | **Time elapsed** (since period start). From 3 min elapsed: band = 1 − dual_limit_price; skip when ask ≥ hedge_price |
| `FOUR_MIN_AFTER_SECONDS` | 240 | **Time elapsed**. From 4 min elapsed: if ask ≥ 0.85 buy now; else keep trailing (band = 1 − limit_price, trigger lowest_ask+0.03) |
| Band 2-min | 1 − dual_limit_price − 0.1 | 2-min: “in band” when ask &lt; this |
| Band 3-min | 1 − dual_limit_price | 3–4 min: “in band” when ask &lt; this |
| `DEFAULT_HEDGE_PRICE` | 0.85 | 3–4 min skip when ask ≥ this; from 4 min buy when ask ≥ this (config overrides) |
| `BAND_2MIN_OFFSET` | 0.1 | 2-min band = 1 − limit − this |
| `NEW_MARKET_PLACE_WINDOW_SECONDS` | 15 | Only place limit orders when time_remaining ≥ period − 15 (first 15 s of period) |

---

## Trailing status on price line (5m BTC)

When one side is filled and we're in the trailing block, the price line (and history.toml) shows: **trail: {Up|Down} ask=X.XX low=X.XX trig=X.XX band=X.XX** — current unfilled ask, lowest ask seen, trigger price (lowest + 0.03), and band threshold.

---

## Flow summary (5m BTC)

1. **New market detected only:** Place BTC Up and Down limit buys at $0.45 (same size) **only when a new period is discovered** — i.e. within the first **15 seconds** of the period (`NEW_MARKET_PLACE_WINDOW_SECONDS`). If you start the bot mid-period (e.g. 2 min into a 5m market), it will **not** place orders for that period; it will wait for the **next** period to start, then place.
2. **As soon as one side fills:** Cancel the opposite limit; **trailing starts immediately**. Before 3 min: band 0.45, trigger when ask ≥ lowest_ask + 0.03. 3–4 min: band 0.50, skip when ask ≥ 0.85 (no lower bound). **From 4 min:** if ask ≥ 0.85 → market buy now; if ask &lt; 0.85 → **keep trailing** (band 0.50, trigger when ask ≥ lowest_ask + 0.03, e.g. buy when price bounces from 0.41 to 0.44). If hedged before 3 min → `two_min_hedge_markets`; if at/after 3 min (including 4-min) → `hedge_executed_for_market`.

No standard “10-min” hedge and no low-price exit (0.05/0.99). **When one side fills:** the opposite limit is cancelled immediately and **trailing runs right away** (no time gate). Limit orders are placed **only** when a new period is in its first 15 seconds (not mid-period) (first snapshot for that period). Orders for the **next** period are not placed early; they are placed when that period’s market is detected.
