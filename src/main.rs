mod app;
mod consts;
mod ir;
mod state;
use app::App;
use state::Speed;

use std::rc::Rc;

use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Ticker, Timer};

use esp_idf_svc::log::EspLogger;
use log::info;


#[embassy_executor::main]
async fn main(spawner: Spawner) {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    spawner.must_spawn(create_app(spawner));

    // Keep main task alive
    loop { Timer::after_secs(300).await };
}

#[embassy_executor::task]
async fn create_app(spawner: Spawner) {
    let app = Rc::new(App::new().await.unwrap());
    spawner.spawn(recv_loop(app.clone())).expect("RECV Task spawn failed");
    spawner.spawn(send_loop(app.clone())).expect("SEND Task spawn failed");
    spawner.spawn(idle_loop(app.clone())).expect("IDLE Task spawn failed");
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

#[embassy_executor::task]
async fn fan_timer(app: Rc<App>) {
    loop {
        // Wait for the first value
        let mut hrs = app.timer_signal.wait().await;

        loop {
            if hrs == state::Timer::Off { break }

            // Wait for either the timer to run out, or a new timer value is given
            info!("Starting new fan timer: {hrs:?}");
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
                },
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
            app.b_power.lock().await.wait_for_falling_edge(),
            app.b_speed.lock().await.wait_for_falling_edge(),
            app.b_timer.lock().await.wait_for_falling_edge()
        ).await;

        match res {
            Either3::First(_) => {
                info!("Power button pressed!");
                _state.lock().await.toggle_power();
            }
            Either3::Second(_) => {
                info!("Speed button pressed!");
                let mut state = _state.lock().await;
                let speed = match state.speed_lvl {
                    // Shift from current speed => next speed
                    Speed::Low => 100,
                    Speed::Med => 33,
                    Speed::High => 67,
                };
                state.set_speed(speed).await;
            }
            Either3::Third(_) => {
                info!("Timer button pressed!");
                let mut state = _state.lock().await;

                // skip all this if power is off
                if !state.power { continue }

                let next_timer = match state.timer {
                    state::Timer::Off => state::Timer::T1,
                    state::Timer::T1  => state::Timer::T2,
                    state::Timer::T2  => state::Timer::T4,
                    state::Timer::T4  => state::Timer::T8,
                    state::Timer::T8  => state::Timer::Off,
                };

                // should never fail since power should be on at this point
                state.set_timer(next_timer).await.unwrap();
            }
        }
        app.signal_needs_publish().await;

        // debounce
        Timer::after_millis(200).await;
    }
}
