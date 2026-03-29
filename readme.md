# Prediction Market — Order Matching Engine

A toy order matching engine built in Rust. Users submit orders through an HTTP API, orders are matched by price-time priority, and fill events are broadcast to connected clients over WebSocket.

---

## Architecture Overview

```
┌─────────────────────┐     ┌─────────────────────┐
│   API Server :3000  │     │   API Server :3001  │  ← Multiple instances OK
│                     │     │                     │
│  POST /orders       │     │  POST /orders       │
│  GET  /orderbook    │     │  GET  /orderbook    │
│  GET  /ws           │     │  GET  /ws           │
└────────┬────────────┘     └──────────┬──────────┘
         │  XADD                       │  XADD
         │                             │
         ▼                             ▼
┌─────────────────────────────────────────────────┐
│               Redis Stream: "orders"            │
│   [seq:1 | order_json] → [seq:2 | order_json]  │  ← Total ordering guaranteed
└────────────────────────┬────────────────────────┘
                         │  XREADGROUP (exclusive delivery)
                         │  only one consumer gets each message
                         ▼
              ┌──────────────────────┐
              │    Matcher Process   │  ← Single process, no races
              │                     │
              │  OrderBook (in-mem) │
              │  BTreeMap<price,    │
              │    VecDeque<Order>> │
              │                     │
              │  On each order:     │
              │  1. pop from stream │
              │  2. match()         │
              │  3. publish fills   │
              │  4. write snapshot  │
              │  5. XACK            │
              └──────────┬──────────┘
                         │
          ┌──────────────┴──────────────┐
          │  PUBLISH                    │  SET
          ▼                             ▼
┌──────────────────┐        ┌─────────────────────┐
│  Redis Pub/Sub   │        │  Redis Key:         │
│  channel:"fills" │        │  "orderbook_snap"   │
└────────┬─────────┘        └──────────┬──────────┘
         │ SUBSCRIBE                   │ GET
         │                             │
    ┌────┴──────┐                 ┌────┴──────┐
    │ API :3000 │                 │ API :3001 │
    │ relay to  │                 │ relay to  │
    │ WS clients│                 │ WS clients│
    └───────────┘                 └───────────┘
```

---

## How the Matching Engine Works

### Data Flow for a Single Order

```
Client                API Server              Redis              Matcher
  │                       │                     │                    │
  │  POST /orders         │                     │                    │
  │ {side,price,qty} ────►│                     │                    │
  │                       │  XADD orders *      │                    │
  │                       │  data={order_json} ─►                    │
  │                       │                     │                    │
  │  {"id": 12345} ◄──────│                     │  XREADGROUP ──────►│
  │                       │                     │  (exclusive read)  │
  │                       │                     │                    │
  │                       │                     │        ┌───────────┤
  │                       │                     │        │ book.submit(order)
  │                       │                     │        │ → fills   │
  │                       │                     │        └───────────┤
  │                       │                     │                    │
  │                       │                     │◄── PUBLISH fills ──│
  │                       │                     │◄── SET snapshot ───│
  │                       │                     │◄── XACK ───────────│
  │                       │                     │                    │
  │   fill event ◄────────│◄── SUBSCRIBE ───────│                    │
  │  (WebSocket)          │                     │                    │
```

### Price-Time Priority Matching

The engine implements strict **price-time priority**:

- **Price priority**: best price always matches first (highest bid, lowest ask)
- **Time priority**: within the same price level, earlier orders match first

#### Buy Order Flow

```
Incoming BUY @ price P, qty Q
│
├─ Is there a resting ask with price ≤ P?
│    YES → match against lowest ask first
│           fill_qty = min(taker.qty, maker.qty)
│           fill price = maker.price (the ask)
│           decrement both quantities
│           if maker.qty == 0 → remove from book
│           repeat until no cross or taker.qty == 0
│
└─ Remaining qty > 0 → rest on bid side at price P
```

#### Sell Order Flow

```
Incoming SELL @ price P, qty Q
│
├─ Is there a resting bid with price ≥ P?
│    YES → match against highest bid first
│           fill price = maker.price (the bid)
│           decrement both quantities
│           repeat until no cross or taker.qty == 0
│
└─ Remaining qty > 0 → rest on ask side at price P
```

#### Partial Fill Example

```
Order Book State:
  ASK  102  [ order#3(qty=5) ]
  ASK  101  [ order#1(qty=10), order#2(qty=5) ]   ← best ask
  ─────────────────────────────────────────────
  BID   99  [ order#0(qty=8) ]

Incoming: BUY @ 101, qty=12

Step 1: best ask = 101, taker.price(101) >= ask(101) → CROSS
        match taker vs order#1 (arrived first — time priority)
        fill_qty = min(12, 10) = 10
        → Fill { maker=1, taker=incoming, price=101, qty=10 }
        order#1 exhausted and removed
        taker.qty now = 2

Step 2: best ask still 101 (order#2 remains)
        fill_qty = min(2, 5) = 2
        → Fill { maker=2, taker=incoming, price=101, qty=2 }
        order#2 now has qty=3, stays in book
        taker.qty = 0 → done

Final Book:
  ASK  102  [ order#3(qty=5) ]
  ASK  101  [ order#2(qty=3) ]
  ─────────────────────────────────────────────
  BID   99  [ order#0(qty=8) ]
```

---

## Order Book Data Structure

```rust
struct OrderBook {
    bids: BTreeMap<u64, VecDeque<Order>>,  // highest price = best bid
    asks: BTreeMap<u64, VecDeque<Order>>,  // lowest  price = best ask
}
```

**Why `BTreeMap<price, VecDeque<Order>>`?**

| Operation           | Complexity     | Notes                                  |
| ------------------- | -------------- | -------------------------------------- |
| Insert order        | O(log n)       | n = distinct price levels              |
| Get best bid/ask    | O(1)           | `keys().next_back()` / `keys().next()` |
| Match at best level | O(1) amortized | `VecDeque::pop_front()`                |
| Cancel order        | O(log n)       | not yet implemented                    |

`BTreeMap` keeps price levels sorted automatically. The best bid is always the last key (`next_back()`), the best ask is always the first key (`next()`). No sorting required on match.

`VecDeque` at each price level gives O(1) push-back (new orders) and O(1) pop-front (match/fill), implementing FIFO time priority correctly.

**Alternatives considered and rejected:**

- `Vec + sort` — O(n log n) per match, degrades badly under load
- `HashMap<price>` — O(1) insert but finding best price is O(n)
- `BinaryHeap` — fast peek at single best price but can't efficiently serve a whole price level in FIFO order

---

## Q&A: Required Design Questions

### 1. How does your system handle multiple API server instances without double-matching an order?

**Redis Streams + Consumer Groups.**

Every API server instance appends orders to a Redis Stream via `XADD`. Redis Streams provide a **globally ordered, append-only log**. Order IDs are assigned by each API instance using a `(instance_id << 32) | local_counter` scheme, so they are globally unique without coordination.

The **single matcher process** reads from the stream using `XREADGROUP`. Consumer Groups guarantee **exclusive delivery** — once a message is delivered to the matcher, Redis will not deliver it to any other consumer until it is explicitly acknowledged with `XACK`. Because there is exactly one matcher instance consuming the group, every order is processed exactly once, in total stream order.

This means matching logic never runs inside the API servers. They are purely write-path (XADD) and read-path (GET /orderbook from Redis key). There is no shared mutable state between API instances.

### 2. What data structure did you use for the order book and why?

`BTreeMap<u64, VecDeque<Order>>` — one for bids, one for asks. Explained in detail above. The short version: sorted keys give O(1) best-price access, VecDeque gives O(1) FIFO within a level, BTreeMap insert/delete is O(log n) on price levels not individual orders.

### 3. What breaks first if this were under real production load?

**The single matcher process becomes the bottleneck.** It processes one order at a time from the stream. At high throughput:

- The Redis Stream queue grows unboundedly if the matcher can't keep up
- The matcher's in-memory order book grows without bound (no order expiry, no GC)
- `GET /orderbook` serves a stale snapshot (it's only as fresh as the last processed order)
- The Pub/Sub fan-out for fills is fire-and-forget — slow WebSocket clients can miss events
- No persistence: if the matcher crashes, the in-memory order book is lost entirely (stream messages are re-delivered, but the book state is gone — orders would match against a blank book)

### 4. What would you build next if you had another 4 hours?

In priority order:

1. **Order book persistence** — snapshot the book to Redis on every N orders so crash recovery restores state correctly before re-processing pending stream entries
2. **Cancel orders** — `DELETE /orders/:id`; requires a hashmap of `id → (side, price)` to locate and remove from the book
3. **Order expiry / TTL** — resting orders that never fill should expire
4. **Matcher high-availability** — run a standby matcher using `XAUTOCLAIM` to take over pending messages after a timeout if the primary crashes
5. **Metrics** — fill rate, queue depth, match latency via a `/metrics` Prometheus endpoint

---

## Project Structure

```
prediction-market-engine/
├── Cargo.toml                  # workspace
├── crates/
│   ├── engine/                 # core library: Order, Fill, OrderBook
│   │   └── src/lib.rs
│   ├── matcher/                # single binary: consumes Redis Stream
│   │   └── src/main.rs
│   └── api-server/             # HTTP + WebSocket binary
│       └── src/main.rs
├── docker-compose.yml
└── README.md
```

---

## Running Locally

**Prerequisites:** Docker, Rust (stable)

```bash
# 1. Start Redis
docker compose up -d redis

# 2. Start the matcher (exactly one instance)
REDIS_URL=redis://127.0.0.1:6379 cargo run -p matcher

# 3. Start one or more API servers on different ports
REDIS_URL=redis://127.0.0.1:6379 PORT=3000 cargo run -p api-server
REDIS_URL=redis://127.0.0.1:6379 PORT=3001 cargo run -p api-server

# 4. Submit an order
curl -X POST http://localhost:3000/orders \
  -H 'Content-Type: application/json' \
  -d '{"side":"sell","price":100,"qty":10}'

# 5. Check the order book
curl http://localhost:3000/orderbook

# 6. Connect a WebSocket client to receive fills
wscat -c ws://localhost:3000/ws
```

---

## Running Tests

```bash
cargo test -p engine
```

The engine crate has unit tests covering: resting orders, full fills, partial fills, price-time priority, and sell-against-highest-bid behaviour.

---

## Environment Variables

| Variable    | Default                  | Description                                           |
| ----------- | ------------------------ | ----------------------------------------------------- |
| `REDIS_URL` | `redis://127.0.0.1:6379` | Redis connection string                               |
| `PORT`      | `3000`                   | HTTP/WS listen port (api-server only)                 |
| `RUST_LOG`  | `info`                   | Log level (`trace`, `debug`, `info`, `warn`, `error`) |
