mod app;
mod common;
mod ir;
mod state;

use app::App;
use common::{FIRMWARE_DOWNLOAD_CHUNK_SIZE, FIRMWARE_MAX_SIZE, FIRMWARE_MIN_SIZE};
use edge_http::io::client::Connection;
use edge_nal::{AddrType::IPv4, Dns};
use edge_nal_std::Stack;
use http::{header::ACCEPT, Uri};
use log::{error, info, warn};
use mime::APPLICATION_OCTET_STREAM;
use state::Speed;

use std::{net::SocketAddr, rc::Rc};

use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Ticker, Timer};

use esp_idf_svc::{
    hal::{io::EspIOError, reset::restart},
    log::EspLogger,
    ota::{EspOta, EspOtaUpdate},
    sys::{EspError, ESP_ERR_IMAGE_INVALID, ESP_ERR_INVALID_RESPONSE, ESP_ERR_NOT_FOUND, ESP_FAIL},
};

fn set_ota_valid() {
    let mut ota = EspOta::new().expect("Instantiate EspOta");
    ota.mark_running_slot_valid()
        .expect("Mark app slot as valid");
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    esp_idf_svc::sys::link_patches();
    let _mounted_eventfs = esp_idf_svc::io::vfs::MountedEventfs::mount(5).unwrap();
    EspLogger::initialize_default();
    set_ota_valid();

    spawner.must_spawn(create_app(spawner));

    // Keep main task alive
    loop {
        Timer::after_secs(3600).await
    }
}

#[embassy_executor::task]
async fn create_app(spawner: Spawner) {
    let app = Rc::new(App::new().await.unwrap());
    spawner.spawn(recv_loop(app.clone())).expect("RECV Task spawn failed");
    spawner.spawn(send_loop(app.clone())).expect("SEND Task spawn failed");
    spawner.spawn(idle_loop(app.clone())).expect("IDLE Task spawn failed");
    spawner.spawn(ota_loop(app.clone())).expect("OTA Task spawn failed");
    spawner.spawn(fan_timer(app.clone())).expect("TIME Task spawn failed");
    spawner.spawn(button_loop(app.clone())).expect("BUTTON Task spawn failed");
}

#[embassy_executor::task]
async fn recv_loop(app: Rc<App>) {
    app.mqtt_recv_loop().await.unwrap();
}

#[embassy_executor::task]
async fn send_loop(app: Rc<App>) {
    app.mqtt_send_loop().await.unwrap();
}

#[embassy_executor::task]
async fn idle_loop(app: Rc<App>) {
    let mut t = Ticker::every(Duration::from_secs(60));
    loop {
        t.next().await;
        app.signal_needs_publish().await;
    }
}

// Small wrapper type around EspOtaUpdate just for adding the edge_nal::io::Write trait
struct AsyncEspOtaUpdate<'a>(pub EspOtaUpdate<'a>);

impl esp_idf_svc::io::ErrorType for &mut AsyncEspOtaUpdate<'_> {
    type Error = EspIOError;
}

impl edge_nal::io::Write for &mut AsyncEspOtaUpdate<'_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        EspOtaUpdate::write(&mut self.0, buf)?;

        Ok(buf.len())
    }
}

async fn handle_ota(uri: Uri) -> Result<(), EspError> {
    let stack = Stack::new();

    let ip = match stack.get_host_by_name(uri.host().unwrap(), IPv4).await {
        Ok(it) => it,
        Err(err) => {
            error!("Failed to resolve hostname {uri:?}: {err:?}");
            return esp_err!(ESP_ERR_NOT_FOUND);
        }
    };
    let mut conn_buf = vec![0_u8; 4096];
    let mut conn: Connection<_> = Connection::new(
        conn_buf.as_mut_slice(),
        &stack,
        SocketAddr::new(ip, uri.port_u16().unwrap()),
    );

    if let Err(e) = conn.initiate_request(
        true,
        edge_http::Method::Get,
        uri.path_and_query().unwrap().as_str(),
        &[(ACCEPT.as_str(), APPLICATION_OCTET_STREAM.as_ref())],
    ).await {
        error!("Failed to create request: {e:?}");
        return esp_err!(ESP_FAIL);
    }

    if let Err(e) = conn.initiate_response().await {
        error!("Failed to get response: {e:?}");
        return esp_err!(ESP_FAIL);
    }

    let (resp, _) = conn.split();

    if resp.code != 200 {
        error!("Unexpected HTTP response: {}", resp.code);
        return esp_err!(ESP_ERR_INVALID_RESPONSE);
    }

    // Check firmware size
    let file_size = resp.headers.content_len().unwrap_or_default();
    if file_size <= FIRMWARE_MIN_SIZE {
        error!("Firmware size ({file_size}) is too small!");
        return esp_err!(ESP_ERR_IMAGE_INVALID);
    }
    if file_size > FIRMWARE_MAX_SIZE {
        error!("Firmware size ({file_size}) is too large!");
        return esp_err!(ESP_ERR_IMAGE_INVALID);
    }

    // Start OTA
    let mut ota = EspOta::new()?;
    info!(
        "CURRENT SLOTS (BOOT, RUN, UPD): ({}, {}, {})",
        ota.get_boot_slot()?.label,
        ota.get_running_slot()?.label,
        ota.get_update_slot()?.label
    );

    let mut upd = AsyncEspOtaUpdate(ota.initiate_update()?);
    let mut buf = vec![0; FIRMWARE_DOWNLOAD_CHUNK_SIZE];
    if let Err(e) = esp_idf_svc::io::utils::asynch::copy_len_with_progress(
        conn,
        &mut upd,
        buf.as_mut_slice(),
        file_size,
        |copied, len| {
            info!(
                "OTA progress: {:.2}%",
                100.0 * copied as f32 / (copied + len) as f32
            );
        }
    ).await {
        error!("Error while writing OTA: {e:?}");
        return upd.0.abort();
    }

    // TODO: checksum

    // OTA was successful if we reach this
    upd.0.complete()
}

#[embassy_executor::task]
async fn ota_loop(app: Rc<App>) {
    loop {
        let ota_success = match app.ota_signal.wait().await.parse::<Uri>() {
            Ok(parsed_uri) => handle_ota(parsed_uri).await,
            Err(e) => {
                warn!("Failed to parse OTA URI: {e:?}");
                esp_err!(ESP_FAIL)
            }
        };

        match ota_success {
            Ok(_) => {
                app.mqtt_log("OTA download successful! rebooting to new image...".to_string()).await;
                restart();
            }
            Err(_) => app.mqtt_log("OTA failed!".to_string()).await,
        };
    }
}

#[embassy_executor::task]
async fn fan_timer(app: Rc<App>) {
    loop {
        // Wait for the first value
        let mut hrs = app.timer_signal.wait().await;

        loop {
            if hrs == state::Timer::Off { break; }

            // Wait for either the timer to run out, or a new timer value is given
            app.mqtt_log(format!("Starting new fan timer: {hrs:?}")).await;
            let res = select(
                Timer::from(hrs),
                app.timer_signal.wait()
            ).await;

            match res {
                Either::First(_) => {
                    // Time finished normally
                    // the fan should have turned off on its own now
                    app.state.clone().lock().await.reset(false);
                    app.signal_needs_publish().await;
                    break; // back to outer loop, wait for new timer
                }
                Either::Second(val) => {
                    // Got a new timer value
                    hrs = val;
                    continue;
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn button_loop(app: Rc<App>) {
    let _state = app.state.clone();
    loop {
        let res = select3(
            app.b_power.lock().await.wait_for_rising_edge(),
            app.b_speed.lock().await.wait_for_rising_edge(),
            app.b_timer.lock().await.wait_for_rising_edge(),
        ).await;

        // debounce, then confirm the button state is still high
        // (hopefully this avoids interference causing a rising edge when nothing was pressed)
        Timer::after_millis(50).await;

        match res {
            Either3::First(_) => {
                if app.b_power.lock().await.is_high() {
                    app.mqtt_log("Power button pressed!".to_string()).await;
                    _state.lock().await.toggle_power();
                }
            }
            Either3::Second(_) => {
                if app.b_speed.lock().await.is_high() {
                    app.mqtt_log("Speed button pressed!".to_string()).await;
                    let mut state = _state.lock().await;
                    let speed = match state.speed_lvl {
                        // Shift from current speed => next speed
                        Speed::Low => 100,
                        Speed::Med => 33,
                        Speed::High => 67,
                    };
                    state.set_speed(speed).await;
                }
            }
            Either3::Third(_) => {
                if app.b_timer.lock().await.is_high() {
                    app.mqtt_log("Timer button pressed!".to_string()).await;
                    let mut state = _state.lock().await;

                    // skip all this if power is off
                    if !state.power {
                        continue;
                    }

                    let next_timer = match state.timer {
                        state::Timer::Off => state::Timer::T1,
                        state::Timer::T1 => state::Timer::T2,
                        state::Timer::T2 => state::Timer::T4,
                        state::Timer::T4 => state::Timer::T8,
                        state::Timer::T8 => state::Timer::Off,
                    };

                    // should never fail since power should be on at this point
                    state.set_timer(next_timer).await.unwrap();
                    app.timer_signal.signal(next_timer);
                }
            }
        }
        app.signal_needs_publish().await;

        // debounce
        Timer::after_millis(200).await;
    }
}
