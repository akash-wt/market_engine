use anyhow::Result;
use engine::{Order, OrderBook};
use redis::{
    AsyncCommands, FromRedisValue, RedisResult, Value,
    streams::{StreamReadOptions, StreamReadReply},
};
use serde_json;
use tracing::{error, info};

const GROUP_NAME: &str = "matcher-group";
const CONSUMER_NAME: &str = "matcher-0";
const FILLS_CHANNEL: &str = "fills";
const ORDERBOOK_KEY: &str = "orderbook_snapshot";
const STREAM_KEY: &str = "orders";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let client = redis::Client::open(redis_url.as_str())?;
    let mut conn = client.get_async_connection().await?;

    let _: RedisResult<Value> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(STREAM_KEY)
        .arg(GROUP_NAME)
        .arg("0")
        .arg("MKSTREAM")
        .query_async(&mut conn)
        .await;

    info!("Matcher started");

    let mut book = OrderBook::new();
    let mut seq: u64 = 0;

    loop {
        let opts = StreamReadOptions::default()
            .group(GROUP_NAME, CONSUMER_NAME)
            .count(100)
            .block(5000);

        let reply: StreamReadReply = match conn.xread_options(&[STREAM_KEY], &[">"], &opts).await {
            Ok(r) => r,
            Err(e) => {
                error!("xread error: {e}");
                continue;
            }
        };

        for stream in reply.keys {
            for entry in stream.ids {
                let Some(payload) = entry
                    .map
                    .get("data")
                    .and_then(|v| String::from_redis_value(v).ok())
                else {
                    error!("no data field in {}", entry.id);
                    let _: RedisResult<()> = conn.xack(STREAM_KEY, GROUP_NAME, &[&entry.id]).await;
                    continue;
                };

                let Ok(mut order) = serde_json::from_str::<Order>(&payload) else {
                    error!("deserialize failed: {payload}");
                    let _: RedisResult<()> = conn.xack(STREAM_KEY, GROUP_NAME, &[&entry.id]).await;
                    continue;
                };

                seq += 1;
                order.seq = seq;

                // using different thread for publising the fills  I/O opration.
                let client_clone = client.clone();
                let fills = book.submit(order);
                tokio::spawn(async move {
                    info!(" fills {:?}",fills);
                    let mut conn = match client_clone.get_async_connection().await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Publich redis conn error ,{:?}", e);
                            return;
                        }
                    };
                    for fill in fills {
                        let msg = match serde_json::to_string(&fill) {
                            Ok(m) => m,
                            Err(e) => {
                                error!("Serialization error ,{:?}", e);
                                continue;
                            }
                        };
                        if let Err(e) = conn.publish::<_, _, ()>(FILLS_CHANNEL, msg).await {
                            error!("Fill Publish error {:?}", e)
                        }
                    }
                });

                let _: () = conn
                    .set(
                        ORDERBOOK_KEY,
                        serde_json::to_string(&serde_json::json!({
                            "bids": book.bids_snapshot(),
                            "asks": book.asks_snapshot(),
                        }))?,
                    )
                    .await?;

                let _: () = conn.xack(STREAM_KEY, GROUP_NAME, &[&entry.id]).await?;
                info!("order {} seq={} acked", entry.id, seq);
            }
        }
    }
}
