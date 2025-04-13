use crate::common::*;
use crate::ir;
use crate::state;

use std::cell::RefCell;
use std::rc::Rc;

use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, mutex::Mutex, signal::Signal,
};
use embassy_time::Timer;
use embedded_svc::wifi::{ClientConfiguration, Configuration};

use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        gpio::{AnyIOPin, Input, PinDriver, Pull},
        modem::Modem,
        prelude::Peripherals,
        reset::restart,
    },
    mqtt::client::{
        EspAsyncMqttClient, EspAsyncMqttConnection, EventPayload, MqttClientConfiguration,
        MqttProtocolVersion, QoS,
    },
    nvs::EspDefaultNvsPartition,
    sys::{esp_crt_bundle_attach, esp_mac_type_t_ESP_MAC_WIFI_STA, esp_read_mac, EspError},
    timer::{EspTaskTimerService, EspTimerService},
    wifi::{AsyncWifi, EspWifi},
};

use ha_mqtt_discovery::{
    mqtt::{
        common::{Availability, AvailabilityCheck, Device, EntityCategory, Qos},
        fan::Fan,
        select::Select,
    },
    Entity,
};

use log::{error, info, warn};
use serde_json::Value;
use static_str_ops::static_format;

#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    mqtt_host: &'static str,
    #[default("")]
    mqtt_user: &'static str,
    #[default("")]
    mqtt_pass: &'static str,
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_pass: &'static str,
}

fn get_mac() -> [u8; 6] {
    let mut mac: [u8; 6] = [0; 6];
    unsafe { esp_read_mac(mac.as_mut_ptr(), esp_mac_type_t_ESP_MAC_WIFI_STA); }
    mac
}

pub enum SignalType {
    Discover,
    Publish,
    Resubscribe,
    Log(String)
    // Other
}

pub struct App {
    _wifi: Rc<RefCell<EspWifi<'static>>>,
    client_id: &'static str,
    base_topic: &'static str,
    mqtt_client: Rc<Mutex<CriticalSectionRawMutex, EspAsyncMqttClient>>,
    mqtt_conn: Rc<Mutex<CriticalSectionRawMutex, EspAsyncMqttConnection>>,
    pub state: Rc<Mutex<CriticalSectionRawMutex, state::Fan>>,
    _signal: Rc<Channel<CriticalSectionRawMutex, SignalType, CHANNEL_SIZE>>,
    pub timer_signal: Rc<Signal<CriticalSectionRawMutex, state::Timer>>,
    pub ota_signal: Rc<Signal<CriticalSectionRawMutex, String>>,

    pub b_power: Mutex<CriticalSectionRawMutex, PinDriver<'static, AnyIOPin, Input>>,
    pub b_speed: Mutex<CriticalSectionRawMutex, PinDriver<'static, AnyIOPin, Input>>,
    pub b_timer: Mutex<CriticalSectionRawMutex, PinDriver<'static, AnyIOPin, Input>>,
}

impl App {
    pub async fn new() -> Result<Self, EspError> {
        let peripherals = Peripherals::take()?;

        let rmt = ir::Remote::new(peripherals.pins.gpio18.into(), peripherals.pins.gpio20.into(), peripherals.rmt)?;

        let mut b_power = PinDriver::input(AnyIOPin::from(peripherals.pins.gpio0))?;
        b_power.set_pull(Pull::Down)?;
        let mut b_speed = PinDriver::input(AnyIOPin::from(peripherals.pins.gpio1))?;
        b_speed.set_pull(Pull::Down)?;
        let mut b_timer = PinDriver::input(AnyIOPin::from(peripherals.pins.gpio2))?;
        b_timer.set_pull(Pull::Down)?;

        let sys_loop = EspSystemEventLoop::take()?;
        let timer_service = EspTimerService::new()?;
        let nvs = EspDefaultNvsPartition::take()?;

        let _wifi = wifi_create(peripherals.modem, &sys_loop, &nvs, &timer_service).await.unwrap_or_else(|_| {
            error!("Wifi setup failed, rebooting...");
            restart()
        });

        let ip_info = _wifi.sta_netif().get_ip_info()?;
        info!("Wifi DHCP info: {:?}", ip_info);

        let client_id = static_format!("esp32c6_{0}", hex::encode(get_mac()));

        let (client, conn) = EspAsyncMqttClient::new(
            CONFIG.mqtt_host,
            &MqttClientConfiguration {
                protocol_version: Some(MqttProtocolVersion::V3_1_1),
                client_id: Some(client_id),
                username: Some(CONFIG.mqtt_user),
                password: Some(CONFIG.mqtt_pass),
                crt_bundle_attach: Some(esp_crt_bundle_attach),
                ..Default::default()
            })?;

        Ok(Self {
            _wifi: Rc::new(RefCell::new(_wifi)),
            client_id,
            base_topic: static_format!("{MQTT_TOPIC}/{0}", client_id), 
            mqtt_client: Rc::new(Mutex::new(client)),
            mqtt_conn: Rc::new(Mutex::new(conn)),
            state: Rc::new(Mutex::new(state::Fan::new(rmt))),
            _signal: Rc::new(Channel::new()),
            timer_signal: Rc::new(Signal::new()),
            ota_signal: Rc::new(Signal::new()),

            b_power: Mutex::new(b_power),
            b_speed: Mutex::new(b_speed),
            b_timer: Mutex::new(b_timer)
        })
    }

    pub async fn mqtt_recv_loop(&self) -> Option<()> {
        while let Ok(e) = self.mqtt_conn.lock().await.next().await {
            match e.payload() {
                EventPayload::Received {
                    id,
                    topic,
                    data,
                    details,
                } => {
                    if let Some(topic) = topic {
                        info!("{id:?} {topic:?} {data:?} {details:?}");
                        match topic.strip_prefix(self.base_topic).unwrap_or(topic) {
                            "/fan/set" => {
                                if let Ok(p) = serde_json::from_slice::<Value>(data) {
                                    self.state.lock().await
                                        .set_power(p["power"].as_bool()?);
                                    self.signal_needs_publish().await;
                                } else {
                                    warn!("Failed to parse json: {data:?}");
                                }
                            },
                            "/fan/speed/set" => {
                                if let Ok(p) = serde_json::from_slice::<Value>(data) {
                                    self.state.lock().await
                                        .set_speed(p["speed"].as_u64()? as u8).await;
                                    self.signal_needs_publish().await;
                                } else {
                                    warn!("Failed to parse json: {data:?}");
                                }
                            },
                            "/fan/timer/set" => {
                                if let Ok(p) = serde_json::from_slice::<Value>(data) {
                                    let res = self.state.lock().await
                                        .set_timer(serde_json::from_value(p["timer"].clone()).unwrap()).await;
                                    self.signal_needs_publish().await;

                                    // Start a new timer if the val was set successfully
                                    if let Ok(t) = res {
                                        self.timer_signal.signal(t);
                                    }
                                } else {
                                    warn!("Failed to parse json: {data:?}");
                                }
                            },
                            "/admin/reboot" => {
                                info!("Reboot requested!");
                                restart();
                            },
                            "/admin/ota" => {
                                let uri = String::from_utf8(data.to_vec()).unwrap();
                                info!("OTA request: {}", uri);
                                self.ota_signal.signal(uri);
                            },
                            "homeassistant/status" => self.signal_needs_publish().await,
                            _ => warn!("Unknown topic: {topic}"),
                        }
                    }
                }

                EventPayload::Disconnected => { 
                    warn!("Network dropped, reconnecting..");
                    while let Err(err) = self._wifi.borrow_mut().connect() {
                        warn!("Reconnect error: {err:?} - retrying..");
                    }
                }

                EventPayload::Connected(_) => {
                    info!("Connected, resubscribing to topics..");
                    self._signal.send(SignalType::Discover).await;
                    self._signal.send(SignalType::Resubscribe).await;
                }

                _ => info!("Unhandled MQTT event: {:?}", e.payload())
            }
        }
        Some(())
    }
    

    pub async fn mqtt_send_loop(&self) -> Result<(), EspError> {
        self.subscribe_topics().await;

        // Send discover and publish initial state when the loop starts
        self._signal.send(SignalType::Discover).await;
        self.signal_needs_publish().await;

        loop {
            match self._signal.receive().await {
                SignalType::Discover => {
                    self.send_discover().await?;
                }

                SignalType::Publish => {
                    let state = self.state.lock().await;
                    let mut attrs = serde_json::to_value(&*state).unwrap();
                    attrs.as_object_mut().unwrap()
                        .insert(String::from("status"), Value::String(String::from("online")));

                    self.mqtt_client.lock().await
                        .publish(format!("{0}/fan/state", self.base_topic).as_str(),
                            QoS::ExactlyOnce,
                            false,
                            serde_json::to_string(&attrs).unwrap().as_bytes()
                        ).await?;
                }

                SignalType::Resubscribe => {
                    self.subscribe_topics().await;
                }

                SignalType::Log(msg) => {
                    self.mqtt_client.lock().await
                        .publish(format!("{0}/log", self.base_topic).as_str(),
                            QoS::AtLeastOnce,
                            false,
                            msg.as_bytes()
                        ).await?;

                }
            }
        }
    }

    async fn send_discover(&self) -> Result<(), EspError> {
        let _device = Device::default()
                      .model("173")
                      .add_identifier(self.client_id)
                      .manufacturer("Vornado");

        let _availability = Availability::single(
                            AvailabilityCheck::topic("~/state")
                                .value_template("{{ value_json.status }}"));

        let entities = [
            Entity::Fan(Fan::default()
                .topic_prefix(format!("{}/fan", self.base_topic))
                .device(_device.clone())
                .icon("mdi:fan")
                .unique_id(format!("{}_fan", self.client_id))
                .object_id(format!("{}_fan", self.client_id))
                .entity_category(EntityCategory::Config)
                .enabled_by_default(true)
                .qos(Qos::ExactlyOnce)
                .availability(_availability.clone())
                .name("Fan")
                

                .state_topic("~/state")
                .state_value_template("{{ value_json.power | string() }}")
                .command_topic("~/set")
                .command_template("{ \"power\": {{ value.lower() }} }")

                .payload_on("True")
                .payload_off("False")

                .percentage_state_topic("~/state")
                .percentage_value_template("{{ value_json.speed }}")
                .percentage_command_topic("~/speed/set")
                .percentage_command_template("{ \"speed\": {{ value }} }")
            ),
            Entity::Select(Select::default()
                .topic_prefix(format!("{}/fan", self.base_topic))
                .device(_device.clone())
                .icon("mdi:fan-clock")
                .unique_id(format!("{}_timer", self.client_id))
                .object_id(format!("{}_timer", self.client_id))
                .entity_category(EntityCategory::Config)
                .enabled_by_default(true)
                .qos(Qos::ExactlyOnce)
                .availability(_availability.clone())
                .name("Auto-off Timer")

                .options(vec!["Off", "1 hr", "2 hr", "4 hr", "8 hr"])
                .state_topic("~/state")
                .value_template("{{ value_json.timer }}")
                .command_topic("~/timer/set")
                .command_template("{ \"timer\": \"{{ value }}\" }")
            )
        ];

        for (topic, payload) in entities.map(|ent| self.create_discover_payload(&ent).unwrap()) {
            self.mqtt_client.lock().await
                .publish(
                    topic.as_str(),
                    QoS::ExactlyOnce,
                    true,
                    payload.as_bytes()
                ).await?;
        }

        Ok(())
    }

    fn create_discover_payload(&self, ent: &Entity) -> Option<(String, String)> {
        let component = ent.get_component_name();
        let attributes = ent.get_attributes().unwrap();
        let object_id = attributes
            .as_object()?
            .get("uniq_id")?
            .as_str()?;
        let topic = format!("{DISCOVERY_PREFIX}/{component}/{object_id}/config");
        let payload = serde_json::ser::to_string(&attributes).unwrap();

        Some((topic, payload))
    }

    pub async fn signal_needs_publish(&self) {
        self._signal.send(SignalType::Publish).await;
    }

    pub async fn mqtt_log(&self, msg: String) {
        info!("[MQTT-LOG] {msg}");
        self._signal.send(SignalType::Log(msg)).await;
    }

    async fn subscribe_topics(&self) {
        let topics = [
            format!("{DISCOVERY_PREFIX}/status"),
            format!("{0}/+/set", self.base_topic),
            format!("{0}/+/+/set", self.base_topic),
            format!("{0}/admin/+", self.base_topic),
        ];

        let client = self.mqtt_client.clone();
        for topic in topics {
            while let Err(e) = client.lock().await.subscribe(topic.as_str(), QoS::ExactlyOnce).await {
                error!("Failed to subscribe to topic \"{topic}\": {e}, retrying...");
                Timer::after_millis(500).await;
            }
        }

    }
}

async fn wifi_create(
    modem: Modem,
    sys_loop: &EspSystemEventLoop,
    nvs: &EspDefaultNvsPartition,
    timer_service: &EspTaskTimerService,
) -> Result<EspWifi<'static>, EspError> {
    let mut esp_wifi = EspWifi::new(modem, sys_loop.clone(), Some(nvs.clone()))?;
    let mut wifi = AsyncWifi::wrap(&mut esp_wifi, sys_loop.clone(), timer_service.clone())?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: CONFIG.wifi_ssid.try_into().unwrap(),
        password: CONFIG.wifi_pass.try_into().unwrap(),
        ..Default::default()
    }))?;

    wifi.start().await?;
    info!("Wifi started");

    while wifi.connect().await.is_err() {
        warn!("Wifi connect failed, retrying..");
        wifi.stop().await?;
        wifi.start().await?;
    }
    info!("Wifi connected");

    wifi.wait_netif_up().await?;
    info!("Wifi netif up");

    Ok(esp_wifi)
}
