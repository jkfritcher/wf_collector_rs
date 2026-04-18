// Copyright (c) 2020, 2026, Jason Fritcher <jkf@wolfnet.org>
// All rights reserved.

use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, EventLoop, Incoming, MqttOptions, QoS,
    StateError,
};

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use crate::common::{MqttArgs, WFSource};

const MQTT_KEEPALIVE_DURATION: Duration = Duration::from_secs(30);
const MQTT_MAX_PACKET_SIZE: usize = 1024;
const MQTT_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MQTT_FATAL_RECONNECT_DELAY: Duration = Duration::from_secs(60);

pub fn mqtt_publish_raw_message(
    publisher_tx: &mpsc::UnboundedSender<(String, String)>,
    topic_base: &str,
    msg_source: &WFSource,
    msg_str: &str,
) {
    let topic_suffix = match msg_source {
        WFSource::UDP => "udp_raw",
        WFSource::WS => "ws_raw",
    };
    let msg_topic = format!("{}/{}", topic_base, topic_suffix);

    mqtt_publish_message(publisher_tx, &msg_topic, msg_str);
}

pub fn mqtt_publish_message(
    publisher_tx: &mpsc::UnboundedSender<(String, String)>,
    msg_topic: &str,
    msg_str: &str,
) {
    if let Err(err) = publisher_tx.send((msg_topic.to_string(), String::from(msg_str))) {
        error!("Failed to add message to publisher_tx: {}", err);
    }
}

pub async fn mqtt_publisher(
    mut publisher_rx: mpsc::UnboundedReceiver<(String, String)>,
    args: MqttArgs,
) {
    // Build options for mqtt client
    let mut opts = MqttOptions::new(args.client_id, args.hostname, args.port);
    opts.set_keep_alive(MQTT_KEEPALIVE_DURATION);
    opts.set_max_packet_size(MQTT_MAX_PACKET_SIZE, MQTT_MAX_PACKET_SIZE);

    // Set credentials, if both username and password are specified
    if let (Some(username), Some(password)) = (args.username, args.password) {
        opts.set_credentials(username, password);
    }

    let mut client: Option<AsyncClient> = None;
    let mut eventloop: Option<EventLoop> = None;

    loop {
        #[allow(unused_assignments)]
        let mut disconnect = false;
        #[allow(unused_assignments)]
        let mut sleep_duration: Option<Duration> = None;

        if client.is_none() {
            let (c, el) = AsyncClient::new(opts.clone(), 16);
            client = Some(c);
            eventloop = Some(el);
            info!("Connected to mqtt broker");
        }

        let c = client.as_mut().unwrap();
        let el = eventloop.as_mut().unwrap();

        'publish_loop: loop {
            tokio::select! {
                Some((topic, payload)) = publisher_rx.recv() => {
                    trace!("Received message: {} | {}", topic, payload);
                    if let Err(e) = c.publish(topic, QoS::AtLeastOnce, false, payload).await {
                        error!("Published failed: {}", e);
                        continue;
                    }
                }
                event = el.poll() => {
                    match event {
                        Ok(Event::Incoming(Incoming::Disconnect)) => {
                            info!("Received disconnect from broker.");
                            disconnect = true;
                            sleep_duration = Some(MQTT_RECONNECT_DELAY);
                            break 'publish_loop;
                        }
                        Ok(_) => {} // Normal events, safely ignored
                        Err(e) => {
                            error!("mqtt eventloop error: {:?}", &e);
                            if is_critical_error(&e) {
                                // Check if it's likely a permanent config issue
                                let is_fatal_error = matches!(e,
                                    ConnectionError::ConnectionRefused(ConnectReturnCode::BadUserNamePassword) |
                                    ConnectionError::ConnectionRefused(ConnectReturnCode::NotAuthorized) |
                                    ConnectionError::ConnectionRefused(ConnectReturnCode::BadClientId)
                                );

                                sleep_duration = if is_fatal_error {
                                    Some(MQTT_FATAL_RECONNECT_DELAY)
                                } else {
                                    Some(MQTT_RECONNECT_DELAY)
                                };
                                disconnect = true;
                                break 'publish_loop;
                            }
                        }
                    }
                }
            }
        }

        if disconnect {
            client = None;
            eventloop = None;
        }

        if let Some(duration) = sleep_duration {
            sleep(duration).await;
        }
    }
}

fn is_critical_error(e: &ConnectionError) -> bool {
    match e {
        // === Fatal configuration / auth errors ===
        // These almost never recover by reconnecting
        ConnectionError::ConnectionRefused(code) => {
            match code {
                ConnectReturnCode::BadUserNamePassword
                | ConnectReturnCode::NotAuthorized
                | ConnectReturnCode::BadClientId
                | ConnectReturnCode::RefusedProtocolVersion
                | ConnectReturnCode::ServiceUnavailable => {
                    // These are worth special logging because they usually need config changes
                    error!("FATAL error occurred: {}", &e);
                    true
                }
                _ => true, // treat other refusal codes as critical too
            }
        }

        // Internal MQTT state machine is confused — usually needs full reset
        ConnectionError::MqttState(state_err) => matches!(
            state_err,
            StateError::InvalidState
                | StateError::ConnectionAborted
                | StateError::Deserialization(_)
        ),

        // TLS / certificate / URL / protocol configuration problems
        ConnectionError::Tls(_)
        | ConnectionError::NotConnAck(_)
        | ConnectionError::RequestsDone => true,

        // Everything else (network, timeouts, temporary broker issues, etc.)
        // → Let rumqttc's built-in reconnection handle it by continuing to poll()
        _ => false,
    }
}
