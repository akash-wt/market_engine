use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
};
use engine::{Fill, Order, Side};
use redis::AsyncCommands;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    redis: redis::Client,
    next_id: Arc<AtomicU64>,
    instance_id: u64,
    fill_tx: broadcast::Sender<String>,
}

const STREAM_KEY: &str = "orders";
const FILLS_CHANNEL: &str = "fills";
const ORDERBOOK_KEY: &str = "orderbook_snapshot";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "api_server=info".into()))
        .init();

    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".into())
        .parse()?;
    let instance_id = port as u64;

    let redis_client = redis::Client::open(redis_url.as_str())?;
    let (fill_tx, _) = broadcast::channel::<String>(1024);

    let state = AppState {
        redis: redis_client.clone(),
        next_id: Arc::new(AtomicU64::new(1)),
        instance_id,
        fill_tx: fill_tx.clone(),
    };

    tokio::spawn(subscribe_fills(redis_client, fill_tx));

    let app = Router::new()
        .route("/orders", post(post_order))
        .route("/orderbook", get(get_orderbook))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
    info!("API server listening on :{port}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Deserialize)]
struct OrderRequest {
    side: Side,
    price: u64,
    qty: u64,
}

async fn post_order(
    State(state): State<AppState>,
    Json(req): Json<OrderRequest>,
) -> impl IntoResponse {
    let local = state.next_id.fetch_add(1, Ordering::Relaxed);
    let id = (state.instance_id << 32) | local;

    let order = Order {
        id,
        side: req.side,
        price: req.price,
        qty: req.qty,
        seq: 0,
    };

    let payload = match serde_json::to_string(&order) {
        Ok(s) => s,
        Err(e) => {
            error!("Serialize error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"serialize"})),
            )
                .into_response();
        }
    };

    let mut conn = match state.redis.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(e) => {
            error!("Redis error: {e}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"redis"})),
            )
                .into_response();
        }
    };

    let _: String = match conn.xadd(STREAM_KEY, "*", &[("data", &payload)]).await {
        Ok(id) => id,
        Err(e) => {
            error!("XADD error: {e}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"xadd"})),
            )
                .into_response();
        }
    };

    info!(id, side = ?order.side, price = order.price, qty = order.qty, "Order accepted");
    (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
}

async fn get_orderbook(State(state): State<AppState>) -> impl IntoResponse {
    let mut conn = match state.redis.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(e) => {
            error!("Redis error: {e}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"redis"})),
            )
                .into_response();
        }
    };

    let raw: Option<String> = conn.get(ORDERBOOK_KEY).await.unwrap_or(None);
    let snapshot: Value = match raw {
        Some(s) => serde_json::from_str(&s).unwrap_or(json!({"bids":[],"asks":[]})),
        None => json!({"bids":[],"asks":[]}),
    };

    (StatusCode::OK, Json(snapshot)).into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    let mut rx = state.fill_tx.subscribe();
    info!("WebSocket client connected");
    loop {
        tokio::select! {
            Ok(msg) = rx.recv() => {
                if socket.send(Message::Text(msg.into())).await.is_err() { break; }
            }
            Some(Ok(msg)) = socket.recv() => {
                match msg {
                    Message::Close(_) => break,
                    Message::Ping(d) => { let _ = socket.send(Message::Pong(d)).await; }
                    _ => {}
                }
            }
            else => break,
        }
    }
    info!("WebSocket client disconnected");
}

async fn subscribe_fills(client: redis::Client, tx: broadcast::Sender<String>) {
    loop {
        if let Err(e) = do_subscribe(&client, &tx).await {
            error!("Pub/Sub error: {e}. Reconnecting in 1s…");
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    }
}

async fn do_subscribe(
    client: &redis::Client,
    tx: &broadcast::Sender<String>,
) -> anyhow::Result<()> {
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.subscribe(FILLS_CHANNEL).await?;

    use futures_util::StreamExt;
    let mut stream = pubsub.on_message();

    while let Some(msg) = stream.next().await {
        let payload: String = msg.get_payload()?;
        let fill: Fill = serde_json::from_str(&payload)?;
        let envelope = serde_json::to_string(&json!({"type":"fill","data":fill}))?;
        let _ = tx.send(envelope);
    }
    Ok(())
}
