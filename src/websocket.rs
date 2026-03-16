// Copyright (c) 2020, 2026, Jason Fritcher <jkf@wolfnet.org>
// All rights reserved.

use std::{
    str,
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};

use futures::stream::FusedStream;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep, timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message,
};

use futures_util::{SinkExt, StreamExt};

use serde_json::{Value as JsonValue, json};

use crate::common::{WFAuthMethod, WFMessage, WFSource, WsArgs};
use WFAuthMethod::{ApiKey, AuthToken};

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

const WF_REST_BASE_URL: &str = "https://swd.weatherflow.com/swd/rest";
const WF_WS_URL: &str = "wss://ws.weatherflow.com/swd/data";

const WF_WS_RECV_TIMEOUT: Duration = Duration::from_secs(180);

static WS_CONNECTED: AtomicBool = AtomicBool::new(false);

pub fn is_ws_connected() -> bool {
    WS_CONNECTED.load(Ordering::SeqCst)
}

fn set_ws_connected(connected: bool) {
    WS_CONNECTED.store(connected, Ordering::SeqCst);
}

async fn get_device_ids_with_station_id(url_str: &str, station_id: u32) -> Option<Vec<u32>> {
    debug!("REST URL: {}", url_str);
    info!("Requesting Device IDs for Station ID {}", station_id);
    let resp = match reqwest::get(url_str).await {
        Ok(resp) => resp,
        Err(err) => {
            error!("Received error requesting device_ids: {}", err);
            return None;
        }
    };
    let resp_bytes = match resp.bytes().await {
        Ok(resp_bytes) => resp_bytes,
        Err(err) => {
            error!("Error receiving response text: {}", err);
            return None;
        }
    };
    let resp_obj: JsonValue = match serde_json::from_slice(resp_bytes.as_ref()) {
        Ok(resp_obj) => resp_obj,
        Err(err) => {
            error!("Error json decoding response: {}", err);
            return None;
        }
    };
    if resp_obj["status"]["status_code"] != 0 {
        error!(
            "Received error status: {} - {}",
            resp_obj["status"]["status_code"], resp_obj["status"]["status_message"]
        );
        return None;
    }

    let mut device_ids: Vec<u32> = Vec::new();
    if let Some(devices) = resp_obj["stations"][0]["devices"].as_array() {
        for device in devices {
            debug!("device: {}", device);
            if device["device_type"] == "HB" {
                // Not interested in the hub device
                continue;
            }
            if let Some(device_id) = device["device_id"].as_u64() {
                device_ids.push(device_id as u32);
            } else {
                warn!("device_id for device was not an integer, skipping.");
                continue;
            }
        }
    } else {
        error!("devices in stations response was not an array, aborting!");
        return None;
    }

    Some(device_ids)
}

async fn websocket_connect(
    url_str: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    // Connect to WS endpoint
    let mut ws_stream = match connect_async(url_str).await {
        Ok((ws_stream, ws_response)) => {
            let code = ws_response.status().as_u16();
            if code != 101 {
                error!("Unexpected response code received: {}", code);
                return Err(String::from("Unexpected response code received."));
            }
            ws_stream
        }
        Err(err) => {
            error!("Error connecting to WebSocket server: {}", err);
            return Err(String::from("Error connecting to WebSocket server."));
        }
    };

    // Get initial message from server
    let msg = match ws_stream.next().await {
        Some(Ok(msg)) => msg,
        Some(Err(err)) => {
            error!("Error occurred reading next message: {}", err);
            if let Err(err) = ws_stream.close(None).await {
                error!("Error closing ws_stream: {}", err);
            }
            return Err(String::from("Error while reading next message."));
        }
        None => {
            error!("End of stream found on WS stream. Shutting down stream.");
            if let Err(err) = ws_stream.close(None).await {
                error!("Error closing ws_stream: {}", err);
            }
            return Err(String::from("End of stream encounter, closing stream."));
        }
    };
    trace!("WS Message received: {}", msg);
    match msg.into_text() {
        Ok(txt) => {
            if !txt.contains("connection_opened") {
                error!("WebSocket connection not successful: {}", txt);
                if let Err(err) = ws_stream.close(None).await {
                    error!("Error closing ws_stream: {}", err);
                }
                return Err(String::from("WebSocket connection not successful."));
            }
        }
        Err(err) => {
            error!("Error converting message into string: {}", err);
            return Err(String::from("Error converting message into string."));
        }
    };

    Ok(ws_stream)
}

async fn websocket_send_listen_start<S>(
    ws_stream: &mut WebSocketStream<S>,
    device_ids: &[u32],
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    for device_id in device_ids {
        // Use current epoch time as request id
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Failed to get current epoch time")
            .as_millis() as u64; // The cast won't overflow for millions of years

        // json to send as request
        let request_id = now.to_string();
        let ws_request = json!(
            {"type":"listen_start","device_id":device_id,"id":request_id}
        )
        .to_string();

        // Connection opened, request station observations
        if let Err(err) = ws_stream.send(Message::text(ws_request)).await {
            error!("Error received sending station obs request: {}", err);
            if let Err(err) = ws_stream.close(None).await {
                error!("Error closing ws_stream: {}", err);
            }
            return Err(String::from("Error sending listen_start request."));
        }
        info!("Successfully sent listen_start for device_id {}", device_id);
    }

    Ok(())
}

pub async fn websocket_collector(collector_tx: mpsc::UnboundedSender<WFMessage>, ws_args: WsArgs) {
    let auth_uri_str = match ws_args.auth_method {
        ApiKey(key) => format!("api_key={}", key),
        AuthToken(token) => format!("token={}", token),
    };

    let device_ids;
    if let Some(station_id) = ws_args.station_id {
        let rest_url_str = format!(
            "{}/stations/{}?{}",
            WF_REST_BASE_URL, station_id, auth_uri_str
        );
        device_ids = match get_device_ids_with_station_id(&rest_url_str, station_id).await {
            Some(device_ids) => device_ids,
            None => {
                error!("Failed to get device_ids from station_id.");
                return;
            }
        };
        info!(
            "Received device_ids {:?} for station_id {}",
            device_ids, station_id
        );
    } else {
        device_ids = ws_args.device_ids.unwrap();
        info!("Using device_ids {:?}", device_ids);
    }

    let mut reconnect_delay: u32 = 0;
    loop {
        // Delay before reconnecting if there were previous errors
        set_ws_connected(false);
        if reconnect_delay > 0 {
            sleep(Duration::from_secs(reconnect_delay.into())).await;
            reconnect_delay = if reconnect_delay < 32 {
                reconnect_delay * 2
            } else {
                32
            };
        }

        let ws_url_str = format!("{}?{}", WF_WS_URL, auth_uri_str);
        info!("Connecting to WebSocket server");
        debug!("Connection URL: {}", ws_url_str);
        let mut ws_stream = match websocket_connect(&ws_url_str).await {
            Err(err) => {
                error!("Error received from websocket_connect(): {}", err);
                if reconnect_delay == 0 {
                    reconnect_delay = 1;
                }
                continue;
            }
            Ok(ws_stream) => ws_stream,
        };
        // Reset reconnect delay
        reconnect_delay = 0;

        info!("WebSocket connected successfully.");

        if let Err(err) = websocket_send_listen_start(&mut ws_stream, &device_ids).await {
            error!("Error received from websocket_send_listen_start: {}", err);
            reconnect_delay = 1;
            continue;
        }

        set_ws_connected(true);
        info!("WS finished sending listen_start(s).");

        loop {
            let msg = match timeout(WF_WS_RECV_TIMEOUT, ws_stream.next()).await {
                Err(_err) => {
                    error!("Timeout waiting for next websocket message");
                    break;
                }
                Ok(None) => {
                    warn!("Websocket connection closed");
                    break;
                }
                Ok(Some(Err(err))) => {
                    error!("WebSocket receive error: {:?}", err);
                    continue;
                }
                Ok(Some(Ok(msg))) => msg,
            };

            trace!("WS Message received: {}", msg);
            if msg.is_close() {
                warn!("WebSocket connection closed: {}", msg);
                break;
            }
            if !(msg.is_text()) {
                warn!("WebSocket non-text message received: {}", msg);
                continue;
            }
            debug!("WS Message: {}", msg);
            let msg = WFMessage {
                source: WFSource::WS,
                message: msg.into_data(),
            };
            if let Err(err) = collector_tx.send(msg) {
                error!("Failed to add message to sender: {}", err);
            }
        }

        if !ws_stream.is_terminated() {
            // Close the stream and try to reconnect
            info!("Closing websocket and reconnecting");
            let _ = ws_stream.close(None).await;
        }
    }
}
