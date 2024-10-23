use esp_idf_svc::sys::{EspError, ESP_ERR_INVALID_STATE};
use log::{error, warn};
use serde::{Deserialize, Serialize};

use crate::ir;

const SIGNAL_DELAY: u64 = 100;

#[derive(Serialize)]
pub struct Fan {
    pub power: bool,
    pub speed_lvl: Speed,
    speed: u8,
    pub timer: Timer,

    #[serde(skip_serializing)]
    rmt: ir::Remote
}

impl Fan {
    pub fn new(rmt: ir::Remote) -> Self {
        Self {
            power: false,
            speed_lvl: Speed::High,
            speed: 100,
            timer: Timer::Off,
            rmt
        }
    }

    pub fn set_power(&mut self, val: bool) {
        match (self.power, val) {
            (false, true) => self.power_on(),
            (true, false) => self.power_off(),
            _ => {}
        }
    }

    pub fn toggle_power(&mut self) {
        self.set_power(!self.power);
    }

    pub fn reset(&mut self, power: bool) {
        self.power = power;
        self.speed_lvl = Speed::High;
        self.speed = 100;
        self.timer = Timer::Off;
    }

    fn power_on(&mut self) {
        self.reset(true);
        if self.rmt.bl_send(ir::POWER).is_err() {
            error!("Failed to send IR code");
        }
    }

    fn power_off(&mut self) {
        self.reset(false);
        if self.rmt.bl_send(ir::POWER).is_err() {
            error!("Failed to send IR code");
        }
    }

    pub async fn set_speed(&mut self, val: u8) {
        if !self.power && val > 0 {
            self.power_on();
            embassy_time::Timer::after_millis(SIGNAL_DELAY).await;
        }

        let tgt_speed = Speed::from_percent(val).unwrap();
        for _ in 0..self.speed_lvl.num_until_speed(tgt_speed) {
            if self.rmt.bl_send(ir::SPEED).is_err() {
                error!("Failed to send IR code");
            }
            embassy_time::Timer::after_millis(SIGNAL_DELAY).await;
        }
        self.speed_lvl = tgt_speed;
        self.speed = val;
    }

    pub async fn set_timer(&mut self, val: Timer) -> Result<Timer, EspError> {
        if !self.power {
            warn!("Fan is off, ignoring timer command");
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_STATE>());
       }

        for _ in 0..self.timer.num_until_time(val) {
            if self.rmt.bl_send(ir::TIMER).is_err() {
                error!("Failed to send IR code");
            }
            embassy_time::Timer::after_millis(SIGNAL_DELAY).await;
        }
        self.timer = val;
        Ok(self.timer)
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Serialize, Debug)]
pub enum Speed {
    //OFF,
    Low = 0,
    Med,
    High,
}

impl Speed {
    pub fn from_percent(pct: u8) -> Option<Speed>{
        match pct {
            //0 => Some(Speed::OFF),
            ..=33 => Some(Speed::Low),
            ..=67 => Some(Speed::Med),
            ..=100 => Some(Speed::High),
            _ => None
        }
    }

    pub fn num_until_speed(&self, tgt: Speed) -> u8 {
        // Speed button goes DOWN one setting
        let n = (*self as i8) - (tgt as i8);
        n.rem_euclid(3) as u8
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq)]
pub enum Timer {
    Off = 0,
    #[serde(rename="1 hr")] T1,
    #[serde(rename="2 hr")] T2,
    #[serde(rename="4 hr")] T4,
    #[serde(rename="8 hr")] T8,
}

impl Timer {
    pub fn num_until_time(&self, tgt: Timer) -> u8 {
        // Timer button goes UP one setting
        let n = (tgt as i8) - (*self as i8);
        n.rem_euclid(5) as u8
    }
}

impl From<Timer> for embassy_time::Timer {
    fn from(value: Timer) -> Self {
        match value {
            Timer::Off => Self::after_ticks(0),
            Timer::T1 => Self::after_secs(1 * 60 * 60),
            Timer::T2 => Self::after_secs(2 * 60 * 60),
            Timer::T4 => Self::after_secs(4 * 60 * 60),
            Timer::T8 => Self::after_secs(8 * 60 * 60),
        }
    }
}
