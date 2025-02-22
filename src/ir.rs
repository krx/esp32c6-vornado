use log::error;

use base64ct::{Base64, Encoding};
use binrw::{helpers::until_eof, io::Cursor, BinRead, BinResult};
use esp_idf_svc::{
    hal::{
        gpio::AnyIOPin,
        rmt::{
            config::CarrierConfig, PinState, Pulse, PulseTicks, RxRmtConfig, RxRmtDriver, Symbol,
            TxRmtConfig, TxRmtDriver, RMT,
        },
    },
    sys::{EspError, ESP_FAIL},
};
use std::cmp;

use crate::esp_err;

const BL_TICK: f64 = 1000. * 269. / 8192.;

#[binrw::parser(reader: r)]
fn bl_parse_ticks() -> BinResult<u16> {
    let mut t = u8::read(r)? as u16;
    if t == 0 {
        t = u16::read_be(r)?;
    }

    Ok(cmp::min((BL_TICK * t as f64) as u16, 0x7FFF))
}

#[derive(BinRead, Debug)]
struct BLSymbol {
    #[br(parse_with=bl_parse_ticks)]
    high: u16,

    #[br(parse_with=bl_parse_ticks)]
    low: u16,
}

#[derive(BinRead, Debug)]
#[br(big)]
struct BLPacket {
    _pkt_type: u8,
    _n_repeats: u8,
    #[br(little)] _len: u16,
    #[br(parse_with=until_eof)] symbols: Vec<BLSymbol>,
    //end: u16 // This ends up globbed into the last `low` pulse. just cap that at 0x7FFF
}

// These b64 strings are taken from either HomeAssistant or a dumped broadlink app db

pub static POWER: &str = "\
JgDyACQOKg4OKioOKg4NKw4qDikPKQ4qDior8SoOKg4NKioOLAwOKg4qDioOKQ4qDioq8ioOKw0NKyoO\
Kg4NKg8pDioOKg4pDykr8CwNKg4OKioOKg4NKw4pDykOKg4qDioq8SsNLAwOKioOLAwOKg4qDioOKQ8p\
Dioq8isNKQ8NKyoOKg4NKg8pDioOKg4pDykr8SsNKg4OKioOKg4NKw4qDikPKQ4qDioq8ioOKg4NKyoO\
Kg4NKg8pDioOKg4qDikr8SsNKg4OKisNKw0OKg4qDioOKQ8pDioq8ioOKg4NKyoOKg4NKw4pDykOKg4q\
DioqAA0F";

pub static TIMER: &str = "\
JgBOACYPJw8MKycPKA8MKgwqDSkoDg0qDCoMAAEDKA4oDg0qJw8nDwwqDSoNKicPDCoNKQ0AAQApDigQ\
CysoDigODSoMKgwqKA4NKQ0qDAANBQ==";

pub static SPEED: &str = "\
JgAEASUOKg4NKisNKw0OKg4qDioOKQ8pKg4OAAEOKw0qDg0qKw0rDQ4qDioOKg4pDykrDQ4AAQ4qDioO\
DSsqDikPDSoPJhEqDioOKSoODgABDioOKg4NKyoOKQ8NKg4qDykOKg4qKQ8NAAEOKw0qDg4qKg4qDg4q\
DikPKQ8pDioqDg0AAQ4rDSsNDiorDSoODioOKg4pDykPKSoODgABDioOKQ8NKyoNKw0OKg8pDioOKg4p\
Kw0OAAEOKw0rDQ4qKg4qDg0rDikPKQ8pDioqDg0AAQ8qDioODSorDSsNDioOKg4qDioOKSsNDgABDioO\
Kg4NKykPKg4NKw4pDykOKg4qKg4NAA0F";

pub fn broadlink_to_signal(blcode: &str) -> Result<impl Iterator<Item = Symbol>, EspError> {
    if let Ok(bytes) = Base64::decode_vec(blcode) {
        if let Ok(pkt) = BLPacket::read(&mut Cursor::new(bytes.as_slice())) {
            return Ok(pkt.symbols.iter().map(|s| {
                    Symbol::new(
                        Pulse::new(PinState::High, PulseTicks::new(s.high).unwrap()),
                        Pulse::new(PinState::Low, PulseTicks::new(s.low).unwrap()),
                    )
                }).collect::<Vec<Symbol>>().into_iter());
        }
        error!("BLPacket parsing failed");
        return esp_err!(ESP_FAIL);
    }
    error!("Failed to decode b64 broadlink string");
    esp_err!(ESP_FAIL)
}

pub struct Remote {
    pub tx: TxRmtDriver<'static>,
    pub _rx: RxRmtDriver<'static>, // Not actually using this in the design for now
}

impl Remote {
    pub fn new(tx_pin: AnyIOPin, rx_pin: AnyIOPin, rmt_dev: RMT) -> Result<Self, EspError> {
        let tx_conf = TxRmtConfig::new().carrier(Some(CarrierConfig::new()));
        let rx_conf = RxRmtConfig::new();

        Ok(Remote {
            tx: TxRmtDriver::new(rmt_dev.channel0, tx_pin, &tx_conf)?,
            _rx: RxRmtDriver::new(rmt_dev.channel2, rx_pin, &rx_conf, 255)?,
        })
    }

    /// Send a base64 encoded broadlink IR code
    pub fn bl_send(&mut self, ircode: &str) -> Result<(), EspError> {
        match broadlink_to_signal(ircode) {
            Ok(syms) => self.tx.start_iter_blocking(syms),
            Err(e) => Err(e),
        }
    }
}
