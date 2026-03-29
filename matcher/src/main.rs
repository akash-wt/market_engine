use anyhow::Result;
use engine::{Order, OrderBook};
use redis::AsyncCommands;
use serde_json;
use tracing::{error, info};

const STREAM_KEY: &str = "orders";
const GROUP_NAME: &str = "matcher-group";
const CONSUMER_NAME: &str = "matcher-0";
const FILLS_CHANNEL: &str = "fills";
const ORDERBOOK_KEY: &str = "orderbook_snapshot";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "matcher=info".into()))
        .init();

    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());

    let client = redis::Client::open(redis_url.as_str())?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    // Create consumer group (ok if already exists).
    let _: Result<(), _> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(STREAM_KEY)
        .arg(GROUP_NAME)
        .arg("0") // start from the very beginning
        .arg("MKSTREAM")
        .query_async(&mut conn)
        .await;

    info!("Matcher started. Consuming from stream '{STREAM_KEY}'…");

    let mut book = OrderBook::new();
    let mut seq: u64 = 0;

    loop {
        // Block up to 2 s waiting for new messages. ">" means "give me only
        // messages that have not been delivered to any consumer yet."
        let results: Vec<redis::streams::StreamReadReply> = redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg(GROUP_NAME)
            .arg(CONSUMER_NAME)
            .arg("COUNT")
            .arg(100)
            .arg("BLOCK")
            .arg(2000)
            .arg("STREAMS")
            .arg(STREAM_KEY)
            .arg(">")
            .query_async(&mut conn)
            .await
            .unwrap_or_default();

        for stream_reply in results {
            for stream_id in stream_reply.keys {
                for entry in stream_id.ids {
                    let raw = match entry.map.get("data") {
                        Some(redis::Value::BulkString(b)) => b.clone(),
                        _ => {
                            error!("Unexpected entry format: {:?}", entry);
                            continue;
                        }
                    };

                    let payload = match std::str::from_utf8(&raw) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("UTF-8 error: {e}");
                            continue;
                        }
                    };

                    let mut order: Order = match serde_json::from_str(payload) {
                        Ok(o) => o,
                        Err(e) => {
                            error!("Deserialize error: {e} — payload: {payload}");
                            let _: Result<(), _> =
                                conn.xack(STREAM_KEY, GROUP_NAME, &[&entry.id]).await;
                            continue;
                        }
                    };

                    // Assign monotonic sequence for time-priority within a
                    // price level. The stream ordering guarantees global FIFO
                    // across all API instances.
                    seq += 1;
                    order.seq = seq;

                    info!(
                        id = order.id,
                        side = ?order.side,
                        price = order.price,
                        qty = order.qty,
                        "Processing order"
                    );

                    let fills = book.submit(order);

                    for fill in &fills {
                        info!(
                            maker = fill.maker_order_id,
                            taker = fill.taker_order_id,
                            price = fill.price,
                            qty = fill.qty,
                            "Fill"
                        );
                        let fill_json = serde_json::to_string(fill)?;
                        let _: () = conn.publish(FILLS_CHANNEL, &fill_json).await?;
                    }

                    // Write orderbook snapshot so GET /orderbook stays current.
                    let snapshot = serde_json::json!({
                        "bids": book.bids_snapshot(),
                        "asks": book.asks_snapshot(),
                    });
                    let _: () = conn
                        .set(ORDERBOOK_KEY, serde_json::to_string(&snapshot)?)
                        .await?;

                    // ACK: tell Redis we processed this message successfully.
                    let _: () = conn.xack(STREAM_KEY, GROUP_NAME, &[&entry.id]).await?;
                }
            }
        }
    }
}
