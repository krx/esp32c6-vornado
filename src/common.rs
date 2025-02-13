use esp_idf_svc::ota::FirmwareInfo;

pub const MQTT_TOPIC: &str = "esp";
pub const DISCOVERY_PREFIX: &str = "homeassistant";
pub const CHANNEL_SIZE: usize = 8;

/// Macro to quickly create EspError from an ESP_ERR_ constant.
#[macro_export]
macro_rules! esp_err {
    ($x:ident) => {
        Err(EspError::from_infallible::<$x>())
    };
}

pub const FIRMWARE_DOWNLOAD_CHUNK_SIZE: usize = 1024 * 20;
pub const FIRMWARE_MAX_SIZE: usize = 0x1f0000; // Max size of each app partition
pub const FIRMWARE_MIN_SIZE: usize = size_of::<FirmwareInfo>() + 1024;
