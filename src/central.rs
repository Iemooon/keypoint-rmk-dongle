#![no_std]
#![no_main]

//! USB receiver role ("dongle"), running on the official Nordic nRF52840
//! Dongle (PCA10059) - flashed as rmk-central.hex via nRF Connect Programmer
//! (app base 0x1000, byte-level proof from the working ZMK receiver image;
//! see memory-dongle.x).
//!
//! Merged verbatim (then adapted) from keypoint-dongle's receiver crate -
//! that port's only real defect was the 0x10000 link base, everything else
//! was sound. Mirrors the ZMK `keypoint_receiver`: no physical matrix, no
//! panel, no pointing hardware. This board is the keymap brain: it runs USB
//! HID to the host, carries the split BLE *central* for both halves
//! (peripheral id 0 = left, id 1 = right), and owns every central-side
//! processor - keyboard, mouse/scroll translation, auto-mouse layer, speed
//! tiers, Vial and storage. Key/encoder/pointing/battery events arrive over
//! the split link already offset into the global 12x8 grid; see
//! `keyboard.toml` and the `PeripheralMatrixConfig` array below.
//!
//! Single-crate merge note: this bin shares src/ with the halves (left.rs /
//! right.rs), so plain `mod` replaces the old #[path] imports; the Makefile
//! swaps the crate-root memory.x before each build (see build.rs).

mod vial;
#[macro_use]
#[allow(unused_macros)] // the matrix-pin macro is for the halves' bins
mod macros;
mod keymap;
mod scroll_key;
mod speed_control;
mod pointer_speed;

use defmt::{info, unwrap};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::mode::Async;
use embassy_nrf::peripherals::{RNG, USBD};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
#[cfg(feature = "softvd")]
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{bind_interrupts, rng, usb};
use nrf_mpsl::Flash;
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use rmk::ble::BleTransport;
use rmk::config::{
    BehaviorConfig, BleBatteryConfig, DeviceConfig, PositionalConfig, RmkConfig, StorageConfig, VialConfig,
};
use rmk::host::HostService;
use rmk::keyboard::Keyboard;
use rmk::processor::builtin::wpm::WpmProcessor;
use rmk::split::PeripheralMatrixConfig;
use rmk::usb::UsbTransport;
use rmk::watchdog::Nrf52Watchdog;
use rmk::{KeymapData, initialize_keymap_and_storage, run_all};
use static_cell::StaticCell;
use vial::{VIAL_KEYBOARD_DEF, VIAL_KEYBOARD_ID};

use rmk::config::AutoMouseLayerConfig;
use rmk::input_device::pointing::{PointingProcessor, PointingProcessorConfig};
use rmk::AutoMouseLayerRunner;

use pointer_speed::{cursor_mode, NUB_ID, PAD_ID};
use scroll_key::{ScrollKeyController, TRACKPOINT_ID};
use speed_control::SpeedController;

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
        // Two split links (left + right halves).
        .central_count(2)?
        // Still a BLE HID peripheral to the host: the receiver can be used
        // wireless too, exactly like the old central half could.
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

// ==================== boot diagnostics ====================
//
// The dongle exposes no debug pins, so failures are reported on BOTH LEDs:
// a panic blinks a group of N quick flashes where N is the boot stage that
// died (see stage() calls in main), repeating forever with a long gap between
// groups. A clean boot instead shows one short flash every ~3 s (heartbeat
// task). LEDs completely dark + no USB enumeration = an await blocked without
// panicking (prime suspect HardwareVbusDetect -> build with --features softvd).

mod bootdiag {
    use core::sync::atomic::{AtomicU8, Ordering};

    pub static STAGE: AtomicU8 = AtomicU8::new(0);

    #[inline]
    pub fn stage(n: u8) {
        STAGE.store(n, Ordering::SeqCst);
    }

    fn delay(mut c: u32) {
        while c != 0 {
            c -= 1;
            core::hint::spin_loop();
        }
    }

    #[panic_handler]
    fn panic(_info: &core::panic::PanicInfo) -> ! {
        let n = STAGE.load(Ordering::Relaxed).max(1) as u32;
        // Raw register access (GPIO0 @ 0x5000_0000): panic-safe by design,
        // no crate types involved. Both dongle LEDs blink together so the
        // report survives a red/green pin mix-up: P0.06 (LED1) cnf @ 0x760,
        // P0.08 (LED2) cnf @ 0x780; OUTSET 0x508 / OUTCLR 0x50C.
        const BOTH: u32 = (1 << 6) | (1 << 8);
        const fn reg(off: usize) -> *mut u32 {
            (0x5000_0000usize + off) as *mut u32
        }
        unsafe {
            core::ptr::write_volatile(reg(0x760), 1);
            core::ptr::write_volatile(reg(0x780), 1);
        }
        loop {
            for _ in 0..n {
                // LEDs are active-LOW on PCA10059 (sink config): CLR lights.
                unsafe { core::ptr::write_volatile(reg(0x50C), BOTH) };
                delay(400_000);
                unsafe { core::ptr::write_volatile(reg(0x508), BOTH) };
                delay(400_000);
            }
            delay(4_000_000);
        }
    }
}

#[embassy_executor::task]
async fn heartbeat(
    mut led1: embassy_nrf::gpio::Output<'static>,
    mut led2: embassy_nrf::gpio::Output<'static>,
) -> ! {
    loop {
        led1.set_low();
        led2.set_low();
        embassy_time::Timer::after_millis(120).await;
        led1.set_high();
        led2.set_high();
        embassy_time::Timer::after_millis(2900).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    bootdiag::stage(1); // entry
    info!("Hello KeyPoint-DG dongle!");
    // Initialize the peripherals and nrf-sdc controller.
    //
    // DC/DC stays OFF here on purpose: the receiver is USB-powered (no
    // battery benefit) and Config::default() is what first booted on the
    // PCA10059 (2026-09-09 baseline). The halves keep DC/DC on - they run on
    // batteries and have the required passives.
    let p = embassy_nrf::init(embassy_nrf::config::Config::default());
    bootdiag::stage(2); // embassy_nrf::init done
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
    bootdiag::stage(3); // MPSL timeslot init
    let mpsl = MPSL.init(unwrap!(mpsl::MultiprotocolServiceLayer::with_timeslots(
        mpsl_p,
        Irqs,
        lfclk_cfg,
        SESSION_MEM.init(mpsl::SessionMem::new())
    )));
    spawner.spawn(mpsl_task(&*mpsl).unwrap());
    // NOTE: individual cfg'd lets, not a block - the SoftdeviceController
    // borrows rng/sdc_mem, so they must live until run_all.
    #[cfg(not(feature = "bare"))]
    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24, p.PPI_CH25, p.PPI_CH26,
        p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    #[cfg(not(feature = "bare"))]
    let mut rng = rng::Rng::new(p.RNG, Irqs);
    // Sized like the official `nrf52840_ble_split_dongle` example for a
    // 2-central-link stack (the old half-central used 6080 with one link).
    #[cfg(not(feature = "bare"))]
    let mut sdc_mem = sdc::Mem::<15472>::new();
    #[cfg(not(feature = "bare"))]
    {
        bootdiag::stage(4); // SDC controller init
    }
    #[cfg(not(feature = "bare"))]
    let sdc = unwrap!(build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem));

    // Initialize usb driver
    bootdiag::stage(5); // USB driver + vbus detect construction
    #[cfg(not(feature = "softvd"))]
    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));
    #[cfg(feature = "softvd")]
    let driver = {
        static SWVD: StaticCell<SoftwareVbusDetect> = StaticCell::new();
        let vd: &SoftwareVbusDetect = SWVD.init(SoftwareVbusDetect::new(true, true));
        Driver::new(p.USBD, Irqs, vd)
    };

    // Initialize flash
    let flash = Flash::take(mpsl, p.NVMC);

    // Keyboard config - "KeyPoint-DG" distinguishes the dongle build from
    // the wired-central keypoint-rmk firmware (USB product + BLE adv name).
    let keyboard_device_config = DeviceConfig {
        vid: 0x1313,
        pid: 0x1208,
        manufacturer: "ZT",
        product_name: "KeyPoint-DG",
        ..DeviceConfig::default()
    };
    let vial_config = VialConfig::new(VIAL_KEYBOARD_ID, VIAL_KEYBOARD_DEF, &[(0, 0), (1, 1)]);
    // The receiver sits on USB and has no battery of its own. With
    // rmk's `ble_central_only` feature on (this build) the receiver never
    // serves a BLE host, so the old `SPLIT_BATTERY_PERIPHERAL_IDS` GATT
    // relay has no consumer; battery lives on the halves' own panels.
    let ble_battery_config = BleBatteryConfig::new(None, false, None, false);
    let storage_config = StorageConfig {
        start_addr: 0xA0000,
        num_sectors: 6,
        ..Default::default()
    };
    let rmk_config = RmkConfig {
        device_config: keyboard_device_config,
        vial_config,
        ble_battery_config,
        storage_config,
        ..Default::default()
    };

    // Initialize the storage and keymap
    let mut keymap_data = KeymapData::new_with_encoder(keymap::get_default_keymap(), keymap::get_default_encoder_map());
    let mut behavior_config = BehaviorConfig::default();
    behavior_config.morse.enable_flow_tap = true;
    // Flow-tap window: see the same block in keypoint-rmk's central.rs -
    // 40 ms keeps typing rolls snappy without vetoing deliberate layer
    // gestures.
    behavior_config.morse.prior_idle_time = embassy_time::Duration::from_millis(40);
    behavior_config.morse.default_profile = behavior_config.morse.default_profile
        .with_hold_timeout_ms(Some(75))
        .with_mode(Some(rmk::types::morse::MorseMode::HoldOnOtherPress));

    // Auto mouse layer: pointer motion drops into layer 4 (keymap.rs
    // "MOTION"), silence drops back out. Same two entries as keypoint-rmk's
    // central - except now BOTH come from across the split link; rmk relays
    // pointing events with their device id, and this runner runs on the
    // dongle where the keymap lives.
    behavior_config.auto_mouse_layer = {
        let mut entries = heapless::Vec::new();
        entries
            .push(AutoMouseLayerConfig {
                device_id: Some(PAD_ID),
                target_layer: 4, // mouse layer
                timeout: embassy_time::Duration::from_millis(500), // pad leaves the layer 500 ms after the last motion. A click counts as no motion, so a move-then-click must fit in this window.
                threshold: 1,
                deactivate_on_key: false,
                extra_mouse_keys: &[],
                reset_timeout_on_key: false,
            })
            .ok();
        entries
            .push(AutoMouseLayerConfig {
                device_id: Some(NUB_ID),
                target_layer: 4, // mouse layer
                timeout: embassy_time::Duration::from_millis(1000), // nub gets 1 s for the same move-then-click sequence
                threshold: 1, // one count of motion keeps the layer alive; a light nub push is only 1-2 counts
                deactivate_on_key: false,
                extra_mouse_keys: &[],
                reset_timeout_on_key: false,
            })
            .ok();
        entries
    };

    let key_config = PositionalConfig::default();
    bootdiag::stage(6); // flash storage mount + keymap init (first-boot format)
    let (keymap, mut storage) = initialize_keymap_and_storage(
        &mut keymap_data,
        flash,
        &storage_config,
        &mut behavior_config,
        &key_config,
    )
    .await;

    // The Keyboard processor is still needed: it consumes the KeyEvents that
    // the two PeripheralManagers publish (already row-offset into the global
    // 12x8 grid) and runs them through the keymap. There is deliberately no
    // local Matrix task - the receiver has no keys, and rmk's TestMatrix
    // would emit synthetic presses.
    let mut keyboard = Keyboard::new(&keymap);
    let host_service = HostService::new(&keymap, &rmk_config);

    let mut usb_transport = UsbTransport::new(driver, rmk_config.device_config).with_host_service(&host_service);
    // Both halves are split peripherals: left = id 0 covering rows 0..5,
    // right = id 1 covering rows 6..11 of the 12x8 keymap. The array index
    // here IS the peripheral id (rmk's BleTransport binds them positionally).
    #[cfg(not(feature = "bare"))]
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

    // ==================== pointing processors (central-side only) ====================
    //
    // The pad lives on the left half, the nub on the right; both cross the
    // split link as PointingEvents with their device id intact, and only a
    // central-side processor may turn them into HID mouse reports - same
    // rule as in keypoint-rmk, now applied to both devices.

    let mut pad_processor = PointingProcessor::new(
        &keymap,
        PointingProcessorConfig {
            device_id: PAD_ID,
            ..Default::default()
        },
    );
    pad_processor.set_pointing_mode(cursor_mode(PAD_ID));

    let mut tp_processor = PointingProcessor::new(
        &keymap,
        PointingProcessorConfig {
            device_id: TRACKPOINT_ID,
            ..Default::default()
        },
    );
    tp_processor.set_pointing_mode(cursor_mode(NUB_ID));

    // Enters the mouse layer on pointer motion and times back out when idle.
    let mut auto_mouse = AutoMouseLayerRunner::new(&keymap);
    let mut scroll_controller = ScrollKeyController::new(&keymap);

    // Reads the four keymap tier cells and writes the processor multipliers.
    let mut speed_controller = SpeedController::new(&keymap);

    let mut watchdog_runner = Nrf52Watchdog::default_runner(p.WDT);

    // Diagnostics: hand BOTH dongle LEDs to the heartbeat task, then announce
    // "run_all entered" (a later panic blinks stage 7).
    let led1 = embassy_nrf::gpio::Output::new(
        p.P0_06,
        embassy_nrf::gpio::Level::High,
        embassy_nrf::gpio::OutputDrive::Standard,
    );
    let led2 = embassy_nrf::gpio::Output::new(
        p.P0_08,
        embassy_nrf::gpio::Level::High,
        embassy_nrf::gpio::OutputDrive::Standard,
    );
    spawner.spawn(heartbeat(led1, led2).unwrap());
    bootdiag::stage(7);

    // Start
    #[cfg(not(feature = "bare"))]
    run_all!(
        storage,
        usb_transport,
        ble_transport,
        wpm_processor,
        keyboard,
        pad_processor,
        tp_processor,
        scroll_controller,
        auto_mouse,
        speed_controller,
        watchdog_runner
    )
    .await;
    #[cfg(feature = "bare")]
    run_all!(
        storage,
        usb_transport,
        wpm_processor,
        keyboard,
        pad_processor,
        tp_processor,
        scroll_controller,
        auto_mouse,
        speed_controller,
        watchdog_runner
    )
    .await;
}
