#![no_std]
#![no_main]

//! Left half, split-peripheral role (peripheral id 0).
//!
//! Hardware identical to keypoint-rmk's old central half: 6x8 matrix
//! (rows 0..5 of the global keymap - the dongle's `PeripheralMatrixConfig`
//! entry 0 keeps row_offset 0), A320 trackpad, left LPM panel, encoder 0,
//! battery ADC and the half status LED. Role change: the keymap, USB leg and
//! all pointing processors moved to the dongle; everything local here is fed
//! by rmk's split relay (Layer/Wpm/Modifier/Indicator/ConnectionStatus come
//! down the link, Key/Pointing/Battery go up). Structurally this is the
//! right half's file mirrored onto the left pin-out.

#[macro_use]
mod macros;
mod lpm009m360a;
/// Only so the shared renderers.rs compiles: these values are written and
/// read on the dongle, this copy stays at its boot default.
#[allow(dead_code)]
mod pointer_speed;
mod renderers;
mod status_led;
mod trackpad;
mod motion_pin;
mod sleep_watch;
mod tp_diag;
mod usb_diag;
mod capy_art;
mod capy_tick;

use defmt::{info, unwrap};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::gpio::{Input, Output};
use embassy_nrf::interrupt::{self, InterruptExt};
use embassy_nrf::mode::Async;
use embassy_nrf::peripherals::{RNG, SAADC, USBD};
use embassy_nrf::saadc::{self, AnyInput, Input as _, Saadc};
use embassy_nrf::{Peri, bind_interrupts, rng, usb};
use nrf_mpsl::Flash;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use panic_probe as _;
use rmk::config::StorageConfig;
use rmk::debounce::default_debouncer::DefaultDebouncer;
use rmk::futures::future::join3;
use rmk::input_device::adc::{AnalogEventType, NrfAdc};
use rmk::input_device::battery::BatteryProcessor;
use rmk::input_device::rotary_encoder::RotaryEncoder;
use rmk::matrix::Matrix;
use rmk::run_all;
use rmk::split::peripheral::run_rmk_split_peripheral;
use rmk::storage::new_storage_without_keymap;
use rmk::watchdog::Nrf52Watchdog;
use static_cell::StaticCell;
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::twim::{self, Twim};
use rmk::display::DisplayProcessor;
use lpm009m360a::{Lpm009m360a, PanelRot};
use motion_pin::PollWait;
use renderers::LeftScreen;
use trackpad::A320;

bind_interrupts!(struct Irqs {
    TWISPI0 => twim::InterruptHandler<embassy_nrf::peripherals::TWISPI0>;
    SPI2 => spim::InterruptHandler<embassy_nrf::peripherals::SPI2>;
    USBD => usb::InterruptHandler<USBD>;
    SAADC => saadc::InterruptHandler;
    RNG => rng::InterruptHandler<RNG>;
    EGU0_SWI0 => nrf_sdc::mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_sdc::mpsl::ClockInterruptHandler, usb::vbus_detect::InterruptHandler;
    RADIO => nrf_sdc::mpsl::HighPrioInterruptHandler;
    TIMER0 => nrf_sdc::mpsl::HighPrioInterruptHandler;
    RTC0 => nrf_sdc::mpsl::HighPrioInterruptHandler;
});

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

/// How many outgoing L2CAP buffers per link
const L2CAP_TXQ: u8 = 3;

/// How many incoming L2CAP buffers per link
const L2CAP_RXQ: u8 = 3;

/// Size of L2CAP packets
const L2CAP_MTU: usize = 251;

fn build_sdc<'d, const N: usize>(
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut rng::Rng<Async>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    // Peripheral role only: advertise, be connected to, nothing else. The
    // advertised id (0, see `run_rmk_split_peripheral` below) is what places
    // this half into the dongle's row 0..5 slot.
    sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .support_dle_peripheral()
        .support_phy_update_peripheral()
        .support_le_2m_phy()
        .peripheral_count(1)?
        .buffer_cfg(L2CAP_MTU as u16, L2CAP_MTU as u16, L2CAP_TXQ, L2CAP_RXQ)?
        .build(p, rng, mpsl, mem)
}

/// Initializes the SAADC peripheral in single-ended mode on the given pin.
fn init_adc(adc_pin: AnyInput, adc: Peri<'static, SAADC>) -> Saadc<'static, 1> {
    let config = saadc::Config::default();
    let channel_cfg = saadc::ChannelConfig::single_ended(adc_pin.degrade_saadc());
    interrupt::SAADC.set_priority(interrupt::Priority::P3);

    saadc::Saadc::new(adc, Irqs, config, [channel_cfg])
}

fn ble_addr() -> [u8; 6] {
    let ficr = embassy_nrf::pac::FICR;
    let high = u64::from(ficr.deviceid(1).read());
    let addr = high << 32 | u64::from(ficr.deviceid(0).read());
    let addr = addr | 0x0000_c000_0000_0000;
    unwrap!(addr.to_le_bytes()[..6].try_into())
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello RMK BLE left!");
    // Initialize the peripherals and nrf-sdc controller
    let mut nrf_config = embassy_nrf::config::Config::default();
    nrf_config.dcdc.reg0_voltage = Some(embassy_nrf::config::Reg0Voltage::_3V3);
    nrf_config.dcdc.reg0 = true;
    nrf_config.dcdc.reg1 = true;
    let p = embassy_nrf::init(nrf_config);
    let mpsl_p = mpsl::Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);
    let lfclk_cfg = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_RC as u8,
        rc_ctiv: mpsl::raw::MPSL_RECOMMENDED_RC_CTIV as u8,
        rc_temp_ctiv: mpsl::raw::MPSL_RECOMMENDED_RC_TEMP_CTIV as u8,

        accuracy_ppm: 500,
        skip_wait_lfclk_started: mpsl::raw::MPSL_DEFAULT_SKIP_WAIT_LFCLK_STARTED != 0,
    };
    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    static SESSION_MEM: StaticCell<mpsl::SessionMem<1>> = StaticCell::new();
    let mpsl = MPSL.init(unwrap!(mpsl::MultiprotocolServiceLayer::with_timeslots(
        mpsl_p,
        Irqs,
        lfclk_cfg,
        SESSION_MEM.init(mpsl::SessionMem::new())
    )));
    spawner.spawn(mpsl_task(&*mpsl).unwrap());
    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24, p.PPI_CH25, p.PPI_CH26,
        p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    let mut rng = rng::Rng::new(p.RNG, Irqs);
    let mut sdc_mem = sdc::Mem::<4696>::new();
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem));

    // Initialize the ADC: one channel for battery level.
    let adc_pin = p.P0_03.degrade_saadc();
    let saadc = init_adc(adc_pin, p.SAADC);
    // Wait for ADC calibration.
    saadc.calibrate().await;

    let (row_pins, col_pins) = config_matrix_pins_nrf!(peripherals: p, input: [P0_24, P0_17, P0_16, P1_08, P0_31, P0_29], output:  [P0_13, P0_15, P0_19, P0_22, P0_20, P1_00, P0_28, P0_30]);

    // Initialize flash
    // nRF52840's bootloader starts from 0xF4000(976K)
    let storage_config = StorageConfig {
        start_addr: 0xA0000, // 640K
        num_sectors: 32,     // 128K
        ..Default::default()
    };
    let flash = Flash::take(mpsl, p.NVMC);
    let mut storage = new_storage_without_keymap(flash, storage_config).await;

    // Initialize the peripheral matrix
    let debouncer = DefaultDebouncer::new();
    let mut matrix = Matrix::<_, _, _, 6, 8, true>::new(row_pins, col_pins, debouncer);

    // Encoder id 0 (was the old central's; id assignment unchanged).
    let pin_a = Input::new(p.P0_14, embassy_nrf::gpio::Pull::None);
    let pin_b = Input::new(p.P0_11, embassy_nrf::gpio::Pull::None);
    let mut encoder = RotaryEncoder::with_resolution(pin_a, pin_b, 4, false, 0);

    // Battery monitoring - ADC on P0.03, same hardware as keypoint-rmk.
    // Sampling every 10 min: the display only needs single-digit percent
    // resolution per sitting, and 12 s one-shot reads multiplied the visible
    // noise (see keypoint-rmk central.rs for the full rationale).
    let mut adc_device = NrfAdc::new(
        saadc,
        [AnalogEventType::Battery],
        [0],
        embassy_time::Duration::from_secs(600),
        None,
    );
    // Full-charge point calibrated to 4.15 V (matches the other half).
    let mut battery_processor = BatteryProcessor::new(2000, 2840);

    // ==================== TrackPad + left status panel ====================

    // --- A320 trackpad: TWISPI0 on SDA P0.26 / SCL P0.04, MOTION on P0.08 ---
    // (verbatim from keypoint-rmk's central.rs - same board, new role.)
    // nRF TWIM drives EasyDMA from RAM only, so it needs a real write buffer.
    static TWI_TX: StaticCell<[u8; 32]> = StaticCell::new();
    let twi_tx = TWI_TX.init([0u8; 32]);
    let a320_i2c = Twim::new(p.TWISPI0, Irqs, p.P0_26, p.P0_04, twim::Config::default(), twi_tx);
    // PollWait keeps this off the GPIOTE channel budget (the matrix is
    // already spending channels).
    let a320_motion = PollWait::new(Input::new(p.P0_08, embassy_nrf::gpio::Pull::Up));
    let mut a320 = A320::new(0, a320_i2c, a320_motion);
    // No PointingProcessor here on purpose - the dongle owns cursor/scroll
    // translation, and rmk carries these events across the split link with
    // device id 0 intact (exactly how the trackpoint worked before).

    // --- Left panel: SPI2 on SCK P0.27 / MOSI P0.05, active-high CS on P0.12 ---
    static SCREEN_FB: StaticCell<[u8; lpm009m360a::FRAMEBUFFER_LEN]> = StaticCell::new();
    let fb = SCREEN_FB.init([0u8; lpm009m360a::FRAMEBUFFER_LEN]);
    let mut spi_cfg = spim::Config::default(); // mode 0 / MSB first
    spi_cfg.frequency = spim::Frequency::M4;
    let screen_spi = Spim::new_txonly(p.SPI2, Irqs, p.P0_27, p.P0_05, spi_cfg);
    // CS is ACTIVE_HIGH, so the idle level is low.
    let screen_cs = Output::new(
        p.P0_12,
        embassy_nrf::gpio::Level::Low,
        embassy_nrf::gpio::OutputDrive::Standard,
    );
    let screen = Lpm009m360a::new(screen_spi, screen_cs, fb, PanelRot::R270);
    // Layer / WPM / LED state reaches this panel because rmk forwards it over
    // the split link (SplitMessage::Layer / Wpm / Modifier) - requires the
    // `display` feature on ALL three builds (Cargo.toml guarantees it).
    let mut display = DisplayProcessor::with_renderer(screen, LeftScreen { snap: None })
        .with_min_render_interval(embassy_time::Duration::from_millis(150));

    // --- Status LED on P0.07 (PWM0). Meaning: blink until the dongle is
    // attached, dark once linked; host state is the dongle-side mirror's job
    // via ConnectionStatus. ---
    let half_pwm = embassy_nrf::pwm::SimplePwm::new_1ch(p.PWM0, p.P0_07, &status_led::pwm_config());
    spawner.spawn(status_led::custom_led_task(status_led::StatusLed::new(half_pwm)).unwrap());
    spawner.spawn(status_led::central_link_task().unwrap());
    // The dongle's ConnectionStatus is mirrored over the split link and
    // every mirror fires a ConnectionStatusChangeEvent, so usb_diag here
    // tracks the *dongle's* USB leg - LeftScreen shows the wired badge in
    // sync. Our own USB stack never runs.
    spawner.spawn(usb_diag::usb_diag_run().unwrap());
    spawner.spawn(capy_tick::capy_tick_run().unwrap());
    spawner.spawn(sleep_watch::sleep_watch_run().unwrap());

    let mut watchdog_runner = Nrf52Watchdog::default_runner(p.WDT);

    // Start - id 0: the dongle's PeripheralMatrixConfig entry 0 (rows 0..5).
    join3(
        run_all!(
            matrix,
            encoder,
            adc_device,
            storage,
            a320,
            display,
            watchdog_runner
        ),
        run_all!(battery_processor),
        run_rmk_split_peripheral(0, sdc, ble_addr()),
    )
    .await;
}
