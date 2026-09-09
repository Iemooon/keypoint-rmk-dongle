#![no_std]
#![no_main]

mod vial;
#[macro_use]
mod macros;
mod keymap;

use defmt::{info, unwrap};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::interrupt::{self, InterruptExt};
use embassy_nrf::mode::Async;
use embassy_nrf::peripherals::{RNG, USBD};
use embassy_nrf::{bind_interrupts, rng, usb};
use nrf_mpsl::Flash;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use panic_probe as _;
use rmk::ble::BleTransport;
use rmk::config::{BehaviorConfig, DeviceConfig, PositionalConfig, RmkConfig, StorageConfig, VialConfig};
use rmk::host::HostService;
use rmk::keyboard::Keyboard;
use rmk::processor::builtin::wpm::WpmProcessor;
use rmk::split::PeripheralMatrixConfig;
use rmk::usb::UsbTransport;
use rmk::watchdog::Nrf52Watchdog;
use rmk::{KeymapData, initialize_keymap_and_storage, run_all};
use static_cell::StaticCell;
use vial::{VIAL_KEYBOARD_DEF, VIAL_KEYBOARD_ID};

// ============================================================================
// KeyPoint-DG receiver (dongle) - minimal bring-up build.
//
// This is the stock `nrf52840_ble_split_dongle` example with everything the
// official Nordic nRF52840 Dongle does NOT have stripped away: no local key
// matrix, no encoders, no PMW3610 mouse, no battery ADC, no CapsLock LED.
// What remains is exactly the example's radio + USB path:
//     MPSL/SDC -> BleTransport (central of the two halves)
//     USBD     -> UsbTransport (HID to the host)
//     flash    -> storage/vial, watchdog
//
// Deliberate deviations from the example, each one hardware-driven:
//   1. DC/DC disabled (Config::default()): the receiver is USB-powered, so
//      regulator efficiency is pointless - and if the dongle board's DC/DC
//      passives differ from the example's dev kit this removes a whole class
//      of early-boot hangs. Halves keep DC/DC enabled as always.
//   2. No Matrix processor: TestMatrix would emit synthetic presses (see
//      keypoint-dongle receiver notes); the split PeripheralManager is the
//      sole publisher of key events here.
//   3. Device identity = KeyPoint (vid 0x1313 / pid 0x1208), keymap = the
//      real KeyPoint 12x8 (both halves), so Vial editing works from day one.
// ============================================================================

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<USBD>;
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
    sdc::Builder::new()?
        .support_scan()
        .support_central()
        .support_adv()
        .support_peripheral()
        .support_dle_peripheral()
        .support_dle_central()
        .support_phy_update_central()
        .support_phy_update_peripheral()
        .support_le_2m_phy()
        .central_count(2)? // The number of peripherals
        .peripheral_count(1)?
        .buffer_cfg(L2CAP_MTU as u16, L2CAP_MTU as u16, L2CAP_TXQ, L2CAP_RXQ)?
        .build(p, rng, mpsl, mem)
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
    info!("Hello KeyPoint-DG dongle!");
    // Initialize the peripherals and nrf-sdc controller.
    // Deviation 1: plain default config = DC/DC regulators OFF (USB dongle).
    let p = embassy_nrf::init(embassy_nrf::config::Config::default());
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
    let mut sdc_mem = sdc::Mem::<15472>::new();
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem));

    // USB driver. HardwareVbusDetect needs no external pin on the nRF52840 -
    // VBUS is sensed by the on-chip USB regulator (USBREGSTATUS.VBUSDETECT),
    // which the stock dongle wires to the 5V rail. Keep the example default.
    let driver = usb::Driver::new(p.USBD, Irqs, usb::vbus_detect::HardwareVbusDetect::new(Irqs));

    // Initialize flash
    let flash = Flash::take(mpsl, p.NVMC);

    // Keyboard config - KeyPoint identity (deviation 3)
    let keyboard_device_config = DeviceConfig {
        vid: 0x1313,
        pid: 0x1208,
        manufacturer: "ZT",
        product_name: "KeyPoint",
        ..DeviceConfig::default()
    };
    let vial_config = VialConfig::new(VIAL_KEYBOARD_ID, VIAL_KEYBOARD_DEF, &[(0, 0), (1, 1)]);
    let storage_config = StorageConfig {
        start_addr: 0xA0000,
        num_sectors: 6,
        clear_storage: false,
        ..Default::default()
    };
    let rmk_config = RmkConfig {
        device_config: keyboard_device_config,
        vial_config,
        storage_config,
        ..Default::default()
    };

    // Initialize the storage and keymap (full 12x8 KeyPoint map; the receiver
    // itself has no keys - deviation 2).
    let mut keymap_data = KeymapData::new_with_encoder(keymap::get_default_keymap(), keymap::get_default_encoder_map());
    let mut behavior_config = BehaviorConfig::default();
    behavior_config.morse.enable_flow_tap = true;
    let key_config = PositionalConfig::default();
    let (keymap, mut storage) = initialize_keymap_and_storage(
        &mut keymap_data,
        flash,
        &storage_config,
        &mut behavior_config,
        &key_config,
    )
    .await;

    let mut keyboard = Keyboard::new(&keymap);
    let host_service = HostService::new(&keymap, &rmk_config);

    let mut usb_transport = UsbTransport::new(driver, rmk_config.device_config).with_host_service(&host_service);
    // Two peripheral halves: left = id 0 (rows 0..5), right = id 1 (rows 6..11).
    let mut ble_transport = BleTransport::new(
        sdc,
        ble_addr(),
        rmk_config,
        [
            PeripheralMatrixConfig {
                rows: 6,
                cols: 8,
                row_offset: 0,
                col_offset: 0,
            },
            PeripheralMatrixConfig {
                rows: 6,
                cols: 8,
                row_offset: 6,
                col_offset: 0,
            },
        ],
    )
    .with_host_service(&host_service);
    let mut wpm_processor = WpmProcessor::new();

    let mut watchdog_runner = Nrf52Watchdog::default_runner(p.WDT);

    // Start - no matrix/encoder/pointing/battery here, see the banner comment.
    run_all!(
        storage,
        usb_transport,
        ble_transport,
        wpm_processor,
        keyboard,
        watchdog_runner
    )
    .await;
}
