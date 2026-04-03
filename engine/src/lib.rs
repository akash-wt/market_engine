use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use tracing::{ info};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: u64,
    pub side: Side,
    pub price: u64, // integer ticks — no floats
    pub qty: u64,
    /// sequence number assigned by the matcher; determines time priority
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub maker_order_id: u64,
    pub taker_order_id: u64,
    pub price: u64,
    pub qty: u64,
}

#[derive(Debug, Default)]
pub struct OrderBook {
    /*
    BtreeMap sorted map {price,VecDeque<Order>},
    VecDeque <FIFO algo.>
    eg .
    {
       {price (100), VecDeque[100,102,99....]},
       {price (101), VecDeque[104,102,99....]}
    }
     */
    /// price → queue of resting buy orders (best bid = highest key)
    bids: BTreeMap<u64, VecDeque<Order>>,
    /// price → queue of resting sell orders (best ask = lowest key)
    asks: BTreeMap<u64, VecDeque<Order>>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Match the incoming order against resting orders; returns generated fills.
    /// Any unmatched remainder is added to the book.
    pub fn submit(&mut self, mut taker: Order) -> Vec<Fill> {
        info!("submit {:?}", taker);
        let mut fills = Vec::new();

        match taker.side {
            Side::Buy => {
                // Buy taker crosses against the lowest ask (ascending iteration).
                while taker.qty > 0 {
                    // Peek at best ask price.
                    let best_ask_price = match self.asks.keys().next().copied() {
                        Some(p) => p,
                        None => break,
                    };
                    // No cross: taker price is below best ask.
                    if taker.price < best_ask_price {
                        break;
                    }
                    let level = self.asks.get_mut(&best_ask_price).unwrap();
                    let maker = level.front_mut().unwrap();

                    let fill_qty = taker.qty.min(maker.qty);
                    fills.push(Fill {
                        maker_order_id: maker.id,
                        taker_order_id: taker.id,
                        price: maker.price, // fill at maker price
                        qty: fill_qty,
                    });

                    maker.qty -= fill_qty;
                    taker.qty -= fill_qty;

                    if maker.qty == 0 {
                        level.pop_front();
                        if level.is_empty() {
                            self.asks.remove(&best_ask_price);
                        }
                    }
                }
                // Rest any unfilled quantity.
                if taker.qty > 0 {
                    self.bids.entry(taker.price).or_default().push_back(taker);
                }
            }

            Side::Sell => {
                while taker.qty > 0 {
                    // best higest price
                    let best_bid_price = match self.bids.keys().next_back().copied() {
                        Some(p) => p,
                        None => break,
                    };


                    if taker.price > best_bid_price {
                        break;
                    }

                    let level = self.bids.get_mut(&best_bid_price).unwrap();
                    let maker = level.front_mut().unwrap();

                    let fill_qty = taker.qty.min(maker.qty);

                    fills.push(Fill {
                        maker_order_id: maker.id,
                        taker_order_id: taker.id,
                        price: maker.price,
                        qty: fill_qty,
                    });

                    maker.qty -= fill_qty;
                    taker.qty -= fill_qty;

                    if maker.qty == 0 {
                        level.pop_front();
                        if level.is_empty() {
                            self.bids.remove(&best_bid_price);
                        }
                    }
                }
                if taker.qty > 0 {
                    self.asks.entry(taker.price).or_default().push_back(taker);
                }
            }
        }

        fills
    }

    // Snapshot of bids: price → total qty (highest price first).
    pub fn bids_snapshot(&self) -> Vec<PriceLevel> {
        self.bids
            .iter()
            .rev()
            .map(|(&price, q)| PriceLevel {
                price,
                qty: q.iter().map(|o| o.qty).sum(),
            })
            .collect()
    }

    /// Snapshot of asks: price → total qty (lowest price first).
    pub fn asks_snapshot(&self) -> Vec<PriceLevel> {
        self.asks
            .iter()
            .map(|(&price, q)| PriceLevel {
                price,
                qty: q.iter().map(|o| o.qty).sum(),
            })
            .collect()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: u64,
    pub qty: u64,
}

