mod app;
mod common;
mod ir;
mod state;

use app::App;
use common::{FIRMWARE_DOWNLOAD_CHUNK_SIZE, FIRMWARE_MAX_SIZE, FIRMWARE_MIN_SIZE};
use embedded_svc::http::{
    client::{Client, Response},
    Headers, Method,
};
use http::{header::ACCEPT, Uri};
use log::{error, info, warn};
use mime::APPLICATION_OCTET_STREAM;
use state::Speed;

use std::{rc::Rc, sync::Arc, thread};

use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Ticker, Timer};

use esp_idf_svc::{
    hal::reset::restart,
    http::client::{Configuration, EspHttpConnection},
    log::EspLogger,
    ota::EspOta,
    sys::{EspError, ESP_ERR_IMAGE_INVALID, ESP_ERR_INVALID_RESPONSE, ESP_FAIL},
};

fn set_ota_valid() {
    let mut ota = EspOta::new().expect("Instantiate EspOta");
    ota.mark_running_slot_valid()
        .expect("Mark app slot as valid");
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();
    set_ota_valid();

    spawner.must_spawn(create_app(spawner));

    // Keep main task alive
    loop {
        Timer::after_secs(300).await
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

fn handle_ota_resp(mut resp: Response<&mut EspHttpConnection>) -> Result<(), EspError> {
    if resp.status() != 200 {
        error!("Unexpected HTTP response: {}", resp.status());
        return esp_err!(ESP_ERR_INVALID_RESPONSE);
    }

    // Check firmware size
    let file_size = resp.content_len().unwrap_or(0) as usize;
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

    let mut upd = ota.initiate_update()?;
    let mut buf = vec![0; FIRMWARE_DOWNLOAD_CHUNK_SIZE];
    let mut total: usize = 0;
    let ota_res = loop {
        let n = resp.read(&mut buf).unwrap_or_default();
        total += n;

        if n > 0 {
            if let Err(e) = upd.write(&buf[..n]) {
                error!("Failed to write OTA chunk: {e:?}");
                break Err(e);
            }
            info!(
                "OTA progress: {:.2}%",
                100.0 * total as f32 / file_size as f32
            );
        }

        if total >= file_size {
            break Ok(());
        }
    };

    // TODO: checksum

    if ota_res.is_err() || total < file_size {
        error!("Error while writing OTA, aborting");
        error!("Total of {total} out of {file_size} bytes received");
        return upd.abort();
    }

    // OTA was successful if we reach this
    upd.complete()
}

#[embassy_executor::task]
async fn ota_loop(app: Rc<App>) {
    loop {
        let mut ota_success: bool = false;
        match app.ota_signal.wait().await.parse::<Uri>() {
            Ok(parsed_uri) => {
                let signal: Arc<Signal<CriticalSectionRawMutex, bool>> = Arc::new(Signal::new());
                let ref_signal = signal.clone();
                let req_thread = thread::Builder::new()
                    .stack_size(1024 * 6)
                    .spawn(move || -> Result<(), EspError> {
                        let mut client = Client::wrap(
                            EspHttpConnection::new(&Configuration {
                                buffer_size: Some(4096),
                                ..Default::default()
                            })?,
                        );

                        let uri = parsed_uri.to_string();
                        let headers = [(ACCEPT.as_str(), APPLICATION_OCTET_STREAM.as_ref())];
                        let res = match client.request(Method::Get, &uri, &headers) {
                            Ok(req) => {
                                match req.submit() {
                                    Ok(resp) => handle_ota_resp(resp),
                                    Err(e) => {
                                        error!("Failed to send request! {e:?}");
                                        esp_err!(ESP_FAIL)
                                    }
                                }
                            },
                            Err(e) => {
                                error!("Failed to build request! {e:?}");
                                esp_err!(ESP_FAIL)
                            }
                        };

                        // Signal the outer await to exit before this thread is done
                        ref_signal.signal(res.is_ok());
                        res
                    }).expect("Failed to spawn OTA thread");

                // await a signal instead of waiting on the thread so other tasks can keep running
                ota_success = signal.wait().await;
                if let Err(e) = req_thread.join() {
                    error!("Inner OTA thread didn't join properly ???: {e:?}");
                }
            }
            Err(e) => {
                warn!("Failed to parse OTA URI: {e:?}");
            }
        };

        if ota_success {
            app.mqtt_log("OTA download successful! rebooting to new image...".to_string()).await;
            restart();
        } else {
            app.mqtt_log("OTA failed!".to_string()).await;
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
                }
            }
        }
        app.signal_needs_publish().await;

        // debounce
        Timer::after_millis(200).await;
    }
}
