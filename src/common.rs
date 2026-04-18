// Copyright (c) 2020, 2026, Jason Fritcher <jkf@wolfnet.org>
// All rights reserved.

use bytes::Bytes;

use crate::config::Config;

#[derive(Debug, Default, Clone)]
pub struct MqttArgs {
    pub hostname: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub client_id: String,
    pub topic_base: String,
}

impl MqttArgs {
    pub fn new(config: Config) -> Self {
        Self {
            hostname: config.mqtt_hostname,
            port: config.mqtt_port,
            username: config.mqtt_username,
            password: config.mqtt_password,
            client_id: config.mqtt_client_id,
            topic_base: config.mqtt_topic_base,
        }
    }
}

#[derive(Debug)]
pub struct WsArgs {
    pub auth_method: WFAuthMethod,
    pub station_id: Option<u32>,
    pub device_ids: Option<Vec<u32>>,
}

impl WsArgs {
    pub fn new(
        auth_method: WFAuthMethod,
        station_id: Option<u32>,
        device_ids: Option<Vec<u32>>,
    ) -> Self {
        Self {
            auth_method,
            station_id,
            device_ids,
        }
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, PartialEq)]
pub enum WFSource {
    UDP,
    WS,
}

#[derive(Debug)]
pub struct WFMessage {
    pub source: WFSource,
    pub message: Bytes,
}

#[derive(Debug)]
pub enum WFAuthMethod {
    ApiKey(String),
    AuthToken(String),
}
