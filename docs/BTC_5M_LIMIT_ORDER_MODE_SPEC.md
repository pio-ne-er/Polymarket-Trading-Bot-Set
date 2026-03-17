# BTC 5-Minute Dual-Limit Strategy — Limit-Order Mode Specification

This document describes the **trading logic of the BTC 5-minute strategy when `dual_limit_trailing_buy_mode = false`** (limit-order mode). It is intended for **paper testing with historical data**: BTC 5m Up/Down token prices and (optionally) Binance BTC price history.

---

## 1. Overview

- **Market:** Polymarket BTC 5-minute Up/Down binary market. Each period is 300 seconds (5 minutes). One token pays $1 if BTC goes up over the period, the other if it goes down.
- **Mode:** Limit-order mode (not trailing-buy). The bot places **two limit buy orders** at a fixed price as soon as a new period is detected. When **exactly one side fills**, it cancels the other limit and then **trails the unfilled side** with time-based bands and a bounce trigger until it buys at market or the period ends.
- **No re-entry:** After the hedge (market buy on the unfilled side) is executed, the bot does not place any further orders for that period. It never places “re-entry” limit orders (e.g. at $0.05) after hedging.

---

## 2. Time Conventions

- **Period duration:** `PERIOD_DURATION_5M = 300` seconds.
- **Time remaining:** Seconds until period end (from the market snapshot).  
  `time_remaining_seconds` is what the system uses for “time left in the candle.”
- **Time elapsed:** Seconds since period start.  
  `time_elapsed_seconds = 300 - time_remaining_seconds`.

All strategy windows are defined in terms of **elapsed** time:

| Constant | Value (seconds) | Meaning |
|----------|----------------|---------|
| (0–2 min) | 0 ≤ elapsed < 120 | **2-minute window**: use 2-min band and 2-min trailing rules. |
| (2–3 min) | 120 ≤ elapsed < 180 | Still in “before 3 min” for some logic; band switches at 180. |
| **THREE_MIN_AFTER_SECONDS** | 180 | **3-minute window** starts: use 3-min band. |
| **FOUR_MIN_AFTER_SECONDS** | 240 | **4-minute window** starts: use 4-min override and 4-min trailing. |

So in your backtester:

- **2-min window:** `time_elapsed_seconds < 180` (strictly before 3 min).
- **3-min window:** `180 ≤ time_elapsed_seconds < 240`.
- **4-min window:** `time_elapsed_seconds ≥ 240`.

---

## 2.5 Price to beat and BTC price difference

For paper testing and analysis you typically align each 5m market with a **BTC spot price series** (e.g. Binance BTC/USDT). Two derived values are useful:

### Price to beat

- **Definition:** The **price to beat** for a given 5m market (period) is the **BTC price at the moment when 0 seconds have elapsed** in that market — i.e. at **period start**.
- **In practice:** For period timestamp `P`, that is the BTC price at wall-clock time `P` (or the first available BTC price in your series with timestamp ≥ `P`). So for each period you store one “price to beat” value (e.g. from the first candle or first tick of that period).
- **Use:** It is the reference level for “did BTC go up or down during this 5 minutes?”. The market resolves Up if BTC is above this at period end, Down if below.

### Difference (current BTC vs price to beat)

- **Definition:** For any snapshot within the period,  
  **difference = current_btc_price − price_to_beat**
- **Interpretation:**
  - **Difference > 0:** BTC is above the period open (bullish for that 5m window).
  - **Difference < 0:** BTC is below the period open (bearish).
  - **Difference = 0:** No change from period start (at the reference price).
- **In backtesting:** At each snapshot you have `current_btc_price` (e.g. Binance mid or last). You already have `price_to_beat` for that period. Compute and log/store `difference = current_btc_price - price_to_beat` (and optionally express in percent: `(difference / price_to_beat) * 100`).

### Summary

| Term | Formula / meaning |
|------|-------------------|
| **Price to beat** | BTC price when `time_elapsed_seconds = 0` for that period (period start). |
| **Current BTC price** | BTC price at the current snapshot (e.g. Binance BTC/USDT). |
| **Difference** | `current_btc_price - price_to_beat` (absolute). |
| **Difference %** | `(current_btc_price - price_to_beat) / price_to_beat * 100` (optional). |

---

## 3. Constants and Config (Replicable in Paper Testing)

### 3.1 Hard-coded constants

| Symbol | Value | Description |
|--------|--------|-------------|
| **LIMIT_PRICE** | 0.45 | Limit buy price for both BTC Up and BTC Down. |
| **PERIOD_DURATION_5M** | 300 | Period length in seconds. |
| **NEW_MARKET_PLACE_WINDOW_SECONDS** | 15 | Limit orders are only placed if the market is seen within the first 15 seconds of the period (i.e. `time_remaining_seconds >= 285`). |
| **BAND_2MIN_OFFSET** | 0.10 | Used to define the 2-min band (see below). |
| **DEFAULT_HEDGE_PRICE** | 0.85 | Default “hedge price” when config does not set it. |

### 3.2 Band formulas

Bands are **price thresholds** for the **unfilled token’s ask**. They define “above band” vs “below band” behavior and when we are allowed to update the trailing low or consider a “valid bounce.”

- **2-min band:**  
  `band_2min = 1 - LIMIT_PRICE - BAND_2MIN_OFFSET = 1 - 0.45 - 0.10 = 0.45`
- **3-min band:**  
  `band_3min = 1 - LIMIT_PRICE = 1 - 0.45 = 0.55`

Which band is used depends on elapsed time:

- **Before 3 min** (elapsed < 180): use **band_2min** (0.45). Label: “2-min.”
- **From 3 min to 4 min** (180 ≤ elapsed < 240): use **band_3min** (0.55). Label: “3-min.”
- **From 4 min** (elapsed ≥ 240): still use **band_3min** (0.55) for trailing logic; the “4-min” label is for display and for the special “buy now if ask ≥ hedge_price” rule.

So in code terms:

- `band_2min = 0.45`
- `band_3min = 0.55`
- `band_threshold` = band_2min when in 2-min window, band_3min when in 3-min or 4-min window.

### 3.3 Config-driven parameters (defaults)

| Config key | Default | Description |
|------------|--------|-------------|
| **dual_limit_hedge_price** | 0.85 | Above this unfilled ask, at 4 min we buy at market immediately. Also: in 3-min window only, we *skip* the tick if ask ≥ this (too expensive). |
| **dual_limit_hedge_trailing_stop** | 0.03 | Trailing trigger: buy when `unfilled_ask >= lowest_ask + 0.03`. Also used as bounce threshold below band. |
| **dual_limit_shares** | None | If set, exact number of shares per limit order and per hedge. If None, shares = `fixed_trade_amount / price` (limit price for initial orders, filled price for hedge sizing). |
| **fixed_trade_amount** | 1.0 | Used when dual_limit_shares is not set (USD per order for sizing). |
| **check_interval_ms** | 1000 | Polling interval; in backtest you can treat this as your snapshot frequency. |

For a minimal paper test you can fix:

- `hedge_price = 0.85`
- `hedge_trailing_stop = 0.03`
- `limit_price = 0.45`

---

## 4. Phase 1: New Market Detection and Limit Order Placement

1. **Eligibility:** Only run when `dual_limit_trailing_buy_mode = false` (limit-order mode).
2. **New market window:** Limit orders are placed **only** if the market is first seen within the first 15 seconds of the period:
   - `time_remaining_seconds >= 300 - 15 = 285`  
   i.e. **time_elapsed_seconds ≤ 15**.
3. **Placement:** For the current period, if we have not already placed for this period:
   - Place a **limit buy** for **BTC Up** at price **0.45**.
   - Place a **limit buy** for **BTC Down** at price **0.45**.
4. **Size:** For each order, shares = `dual_limit_shares` if set, else `fixed_trade_amount / 0.45`.
5. **Once per period:** A “last_placed_period” guard ensures we only place once per period.

For backtesting with historical data you typically **assume** these two limit orders are placed at the start of the period (or at the first snapshot within the first 15 s). You then need a **fill model** for limit orders (e.g. fill when mid/ask crosses 0.45, or use your own rule).

---

## 5. Phase 2: One Side Filled — Cancel Unfilled Limit, Start Trailing

- We only proceed if **exactly one** of the two limit orders has filled (e.g. tracked by `buy_order_confirmed` per side).
- **Action:** Cancel the **unfilled** limit order (so we never get two filled sides from limits).
- **State:** From this point on we are in “trailing” state for this period:
  - **Filled side:** We know its **units** (shares) and **purchase_price** (fill price).
  - **Unfilled side:** We will only buy it at **market** when the trailing logic or 4-min rule triggers.

For paper testing:

- You need to know which side filled and at what price/size.
- **hedge_shares** = filled side’s units (we buy the same number of shares of the unfilled token).
- **filled_side_price** = filled side’s purchase price (for logging only; not used in trigger math).

---

## 6. Phase 3: Trailing Logic (Unfilled Side)

After one side is filled we **do not** buy the unfilled side immediately. We **trail** its ask and buy at market only when either:

- **4-min override:** unfilled_ask ≥ hedge_price (e.g. 0.85) and elapsed ≥ 240, or  
- **Bounce trigger:** unfilled_ask ≥ lowest_ask + hedge_trailing_stop (e.g. 0.03).

“Lowest_ask” is the minimum unfilled ask observed since we started trailing (or since we last “reset” it under the band rules below).

### 6.1 When we do *not* buy (wait for bounce or 4-min)

- **4-min not reached and ask ≥ hedge_price (3-min only):**  
  If `180 ≤ elapsed < 240` and `current_ask >= hedge_price`, we **skip** this tick (too expensive). No update to lowest_ask, no buy.
- **Above band, no valid bounce:**  
  If `current_ask >= band_threshold` and we do **not** consider this a “valid bounce” (see below), we only **update** lowest_ask when `current_ask < previous_lowest_ask`, then return (no buy).
- **Below bounce trigger:**  
  If `current_ask < lowest_ask + hedge_trailing_stop`, we do **not** buy; we may update lowest_ask (see below). Then return.

### 6.2 Updating lowest_ask

- **First time in trailing (no stored lowest):** Set `lowest_ask = current_ask`.
- **When current_ask is below band:**  
  Update lowest only if price goes down:  
  `if current_ask < lowest_ask then lowest_ask = current_ask`.
- **When current_ask is at or above band:**  
  - If we are in “valid bounce” (see next subsection), we do **not** update lowest_ask here; we allow the buy path.
  - Otherwise we only update when price drops:  
    `if current_ask < lowest_ask then lowest_ask = current_ask`, then return (no buy).

So: lowest_ask is **never increased**; it only moves down when we see a lower ask.

### 6.3 “Valid bounce” (allow buy above band)

We are allowed to buy **above** the band if the price had previously been **at or below** a “bounce threshold” and has now come back up. Specifically:

- **Bounce threshold:**  
  `bounce_threshold = band_threshold - hedge_trailing_stop`  
  - 2-min: `0.45 - 0.03 = 0.42`  
  - 3-min/4-min: `0.55 - 0.03 = 0.52`
- **Condition:**  
  If `current_ask >= band_threshold` **and** `lowest_ask <= bounce_threshold`, we treat this as a valid bounce: we **allow** the code path that can lead to a buy (we still require `current_ask >= lowest_ask + hedge_trailing_stop` to actually buy).

So in the 2-min window, if lowest_ask ever reached 0.42 or below, then later if ask is ≥ 0.45 we can trigger. In the 3-min window, if lowest_ask reached 0.52 or below, then ask ≥ 0.55 can trigger (subject to the trigger condition).

### 6.4 Trailing trigger (actual buy condition)

We **execute a market buy** on the unfilled side when **all** of the following hold:

1. We did not already hedge this period (hedge_executed_for_market).
2. In 3-min window only: we did not skip due to ask ≥ hedge_price.
3. Either:
   - **4-min override:** `time_elapsed_seconds >= 240` and `unfilled_ask >= hedge_price`, or  
   - **Bounce trigger:** `unfilled_ask >= lowest_ask + hedge_trailing_stop` (and we’re in a valid state for trailing; see above).
4. (Live only) Balance check: we don’t already have ≥ hedge_shares of the unfilled token.

So for paper testing the **trailing buy condition** is:

- If **elapsed ≥ 240** and **ask ≥ 0.85** → buy at market (4-min override).
- Else if **ask ≥ lowest_ask + 0.03** (and we’re past the “above band” no-buy logic and valid-bounce logic) → buy at market (2-min or 3-min or 4-min trailing).

After a buy we mark the period as hedged and stop all further orders for that period.

---

## 7. Phase 4: 4-Minute Override (Detail)

- **When:** `time_elapsed_seconds >= 240`.
- **If unfilled_ask >= hedge_price (e.g. 0.85):**  
  Buy the unfilled side at **market** immediately (same tick). No need to wait for a bounce. Shares = hedge_shares; cost = hedge_shares * current_ask.
- **If unfilled_ask < hedge_price:**  
  Do **not** buy this tick; continue with normal trailing (update lowest_ask, check for bounce trigger on later ticks).

So at 4 min we only “force” a buy when the ask is already high; otherwise we keep trailing until ask ≥ lowest_ask + 0.03.

---

## 8. Order Sizing Summary

- **Limit orders (initial):**  
  Price = 0.45.  
  Shares = `dual_limit_shares` if set, else `fixed_trade_amount / 0.45`.
- **Hedge (market buy on unfilled side):**  
  Shares = **same as filled side** (hedge_shares = filled side’s units).  
  Cost = hedge_shares * unfilled_ask at the moment of the buy.  
  In the code, when dual_limit_shares is used for the filled side, the hedge uses that same share count; when it’s derived from fixed_trade_amount/price, the filled side’s units are used.

For paper testing you can assume: **hedge_shares = number of shares of the filled side**, and we buy **hedge_shares** of the unfilled token at the **current ask** when the buy triggers.

---

## 9. State to Track Per Period (Backtest)

For each 5-minute period, maintain:

1. **Period id** (e.g. period_timestamp / condition_id).
2. **Limit orders:** Placed at t=0 (or first 15 s). Which side (if any) filled, at what price and size, and when.
3. **Unfilled side:** Which token is unfilled (Up or Down).
4. **lowest_ask:** Minimum unfilled ask seen since trailing started (or since last “above band” update). Initialize to first unfilled ask after cancel.
5. **hedge_executed:** Boolean; once true, no more orders.
6. **two_min_hedge_markets:** If we triggered a buy in the 2-min window, add this period so we don’t re-enter (in the code this set is used to skip certain logic after a 2-min hedge; for a single-hedge-per-period backtest you may just use hedge_executed).

Time variables you need per snapshot:

- `time_elapsed_seconds` (or equivalently `time_remaining_seconds`).
- `unfilled_ask` (and optionally bid) for the unfilled token.

---

## 10. Pseudocode for One Tick (Limit-Order Mode, After One Side Filled)

```text
assume: one side filled (e.g. Down), unfilled = Up
        hedge_shares = filled_side.units
        hedge_price = 0.85, hedge_trailing_stop = 0.03
        band_2min = 0.45, band_3min = 0.55

elapsed = 300 - time_remaining_seconds
run_3min = (elapsed >= 180)
run_4min = (elapsed >= 240)

if hedge_executed: return

# 4-min override: buy now if ask is high
if run_4min and unfilled_ask >= hedge_price:
    execute market buy (hedge_shares at unfilled_ask)
    set hedge_executed = true
    return

# 3-min only: skip tick if too expensive
if run_3min and not run_4min and unfilled_ask >= hedge_price:
    return

band_threshold = (run_4min or run_3min) ? band_3min : band_2min
bounce_threshold = band_threshold - hedge_trailing_stop

# Above band: update low or allow valid bounce
if unfilled_ask >= band_threshold:
    if lowest_ask <= bounce_threshold:
        allow_buy_above_band = true   # fall through to trigger check
    else:
        if unfilled_ask < lowest_ask:
            lowest_ask = unfilled_ask
        return

# Update lowest when below band
if unfilled_ask < lowest_ask:
    lowest_ask = unfilled_ask

# Trigger: buy when ask >= lowest + 0.03
if unfilled_ask >= lowest_ask + hedge_trailing_stop:
    execute market buy (hedge_shares at unfilled_ask)
    set hedge_executed = true
else:
    # waiting for bounce
    pass
```

---

## 11. Summary Table (Quick Reference)

| Item | Value / Rule |
|------|------------------|
| Period length | 300 s |
| Limit price | 0.45 (both sides) |
| Place limits only when | First 15 s of period (time_remaining ≥ 285 s) |
| After one fill | Cancel other limit; trail unfilled ask |
| 2-min band | 0.45 (before 3 min) |
| 3-min band | 0.55 (from 3 min) |
| Hedge price | 0.85 (config) |
| Trailing stop | 0.03 (config) |
| Bounce threshold | band − 0.03 |
| 4-min override | If elapsed ≥ 240 and ask ≥ 0.85 → buy now |
| Trailing trigger | ask ≥ lowest_ask + 0.03 |
| Hedge size | Same shares as filled side |
| No re-entry | After hedge, do nothing for that period |

---

This specification should be enough to reimplement the BTC 5m limit-order mode in a separate backtester using historical Up/Down token prices and (optionally) Binance BTC price history, without referring to the live codebase.
