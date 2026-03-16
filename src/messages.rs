// Copyright (c) 2020, 2026, Jason Fritcher <jkf@wolfnet.org>
// All rights reserved.

use std::str;

use serde_json::Value;
use tokio::sync::mpsc;

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use crate::common::{WFMessage, WFSource};
use crate::mqtt::{mqtt_publish_message, mqtt_publish_raw_message};
use crate::websocket::is_ws_connected;

fn get_hub_sn_from_msg(msg_obj: &serde_json::Value) -> Option<&str> {
    let msg_map = msg_obj.as_object()?;
    if msg_map.contains_key("type") && msg_map.get("type")? == "hub_status" {
        msg_map.get("serial_number")?.as_str()
    } else {
        msg_map.get("hub_sn")?.as_str()
    }
}

pub async fn message_consumer(
    mut collector_rx: mpsc::UnboundedReceiver<WFMessage>,
    publisher_tx: mpsc::UnboundedSender<(String, String)>,
    ignored_msg_types: Vec<String>,
    mqtt_topic_base: String,
) {
    // Wait for messages from the consumers to process
    while let Some(msg) = collector_rx.recv().await {
        let msg_str = match str::from_utf8(&msg.message) {
            Err(err) => {
                error!("Error in str::from_utf8, ignoring message: {}", err);
                continue;
            }
            Ok(msg) => msg,
        };
        let msg_json: Value = match serde_json::from_str(msg_str) {
            Err(err) => {
                error!("Error parsing json, skipping message: {}", err);
                continue;
            }
            Ok(msg) => msg,
        };
        if !msg_json.is_object() {
            warn!(
                "{:?} message is not an expected json object, skipping",
                msg.source
            );
            continue;
        }
        let msg_obj = match msg_json.as_object() {
            Some(msg_obj) => msg_obj,
            None => {
                warn!("msg_json is not an object, skipping.");
                debug!("msg_json = {:?}", msg_json);
                continue;
            }
        };
        let msg_type = match msg_obj.get("type") {
            Some(msg_type) => match msg_type.as_str() {
                Some(msg_type) => msg_type,
                None => {
                    warn!("Received message with a non-string type field, skipping.");
                    debug!("msg_type = {:?}", msg_type);
                    continue;
                }
            },
            None => {
                error!("type key was not found in message, skipping.");
                debug!("msg_obj = {:?}", msg_obj);
                continue;
            }
        };

        // Ignore message types we're not interested in
        if msg_type.ends_with("_debug")
            || msg_type == "ack"
            || ignored_msg_types.iter().any(|x| x == msg_type)
        {
            continue;
        }

        // Ignore cached WS summary messages
        // if msg.source == WFSource::WS {
        //     if let Some(msg_source) = msg_obj.get("source") {
        //         if msg_source.as_str().unwrap_or("") == "cache" {
        //             debug!("Ignoring WS cached summary message");
        //             continue;
        //         }
        //     };
        // }

        // Get the Hub serial number and make topic base
        let hub_sn = match get_hub_sn_from_msg(&msg_json) {
            Some(sn) => sn,
            None => {
                warn!("Received message with no serial numbers, ignoring.");
                debug!("Packet contents: {}", &msg_json);
                continue;
            }
        };
        let topic_base = format!("{0}/{1}", mqtt_topic_base, hub_sn);

        // Publish raw message to mqtt
        mqtt_publish_raw_message(&publisher_tx, &topic_base, &msg.source, msg_str);

        // Get WS connected state
        let ws_connected = is_ws_connected();

        // Handle specific message types
        match msg_type {
            "rapid_wind" => {
                let topic = format!("{}/rapid", &topic_base);
                mqtt_publish_message(&publisher_tx, &topic, msg_str);
            }
            mt if mt.ends_with("_status") => {
                let topic = format!("{}/status", &topic_base);
                mqtt_publish_message(&publisher_tx, &topic, msg_str);
            }
            mt if mt.starts_with("obs_") => {
                if ws_connected && msg.source == WFSource::UDP {
                    // Ignore the UDP events if we're connected via WS
                    continue;
                }
                let topic = format!("{}/obs", &topic_base);
                mqtt_publish_message(&publisher_tx, &topic, msg_str);
            }
            mt if mt.starts_with("evt_") => {
                match mt {
                    "evt_strike" => {
                        if ws_connected && msg.source == WFSource::UDP {
                            // Ignore the UDP events if we're connected via WS
                            continue;
                        }
                    }
                    "evt_precip" => {
                        if msg.source == WFSource::WS {
                            // Ignore the WS event and always use UDP event
                            continue;
                        }
                    }
                    _ => {
                        warn!("Unknown evt message type, ignoring. ({})", mt);
                        continue;
                    }
                }
                let topic = format!("{}/evt", &topic_base);
                mqtt_publish_message(&publisher_tx, &topic, msg_str);
            }
            _ => (),
        }
    }
}
