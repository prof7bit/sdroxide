//! RADE narrows the receive filter to a little more than the carriers of the
//! selected version, on either sideband.
//!
//! Its own test binary: it saves the digi settings, so it points
//! `SDROXIDE_CONFIG_DIR`, which is process-wide, at a directory of its own.

use std::time::Duration;

use sdroxide_radio::{Complex32, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, DigiConfig, Mode, RadeVersion, RadioEvent, RxId, Vfo};

const RATE: f64 = 2_400_000.0;
const RADE_40M: f64 = 7_177_000.0;
const RADE_20M: f64 = 14_236_000.0;

struct MockSource {
    center: f64,
}

impl IqSource for MockSource {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        self.center
    }
    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center = hz;
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(2048);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "mock rx source".into()
    }
}

#[test]
fn the_receive_filter_follows_version_and_sideband() {
    let dir = std::env::temp_dir().join(format!("sdroxide-rade-rx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: the only test in this binary, set before the engine's threads.
    unsafe { std::env::set_var("SDROXIDE_CONFIG_DIR", &dir) };

    let caps = DeviceCaps {
        driver: "mock".into(),
        label: "mock".into(),
        rx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(0.0, 1_000_000_000.0)],
        ..DeviceCaps::default()
    };
    let mut h = start_engine(
        Box::new(MockSource { center: RADE_20M }),
        caps,
        EngineConfig { tx_ham_only: false, ..Default::default() },
    );
    let thread = h.thread.take();
    std::thread::sleep(Duration::from_millis(150));

    let mut last = None;
    let mut passband_after = |cmd: Command| {
        h.cmd_tx.send(cmd).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        while let Ok(ev) = h.event_rx.try_recv() {
            if let RadioEvent::State(s) = ev {
                last = Some((s.rx[0].filter_lo, s.rx[0].filter_hi));
            }
        }
        last.expect("the engine should publish state")
    };
    let tune = |hz| Command::SetVfo { vfo: Vfo::A, hz };
    let version =
        |rade_version| Command::SetDigiConfig(DigiConfig { rade_version, ..DigiConfig::default() });

    passband_after(tune(RADE_20M));
    let mode = Command::SetMode { rx: RxId::Main, mode: Mode::Rade };
    assert_eq!(passband_after(mode), (450.0, 2500.0), "V1, upper");
    assert_eq!(passband_after(tune(RADE_40M)), (-2500.0, -450.0), "V1, lower");
    assert_eq!(passband_after(version(RadeVersion::V2)), (-2100.0, -850.0), "V2, lower");
    assert_eq!(passband_after(Command::SetRadeSideband(false)), (850.0, 2100.0), "own choice");
    assert_eq!(passband_after(tune(RADE_20M)), (850.0, 2100.0), "V2, upper");
    assert_eq!(passband_after(version(RadeVersion::V1)), (450.0, 2500.0), "back on V1");
    // Another mode gets its own passband again.
    let usb = Command::SetMode { rx: RxId::Main, mode: Mode::Usb };
    assert_eq!(passband_after(usb), Mode::Usb.default_filter(), "USB is not narrowed");

    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
