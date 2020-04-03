//! Cargo runner for loading and running Rust programs in a u-boot-less environment

use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use std::{
    env, fs,
    io::{self, Write},
    thread,
};

use anyhow::{bail, format_err};
use image::write::Image;
use serialport::SerialPortSettings;
use xmas_elf::ElfFile;

use usd::Usd;

fn main() -> Result<(), anyhow::Error> {
    // NOTE(skip) program name
    let args = env::args().skip(1).collect::<Vec<_>>();

    if args.len() != 1 {
        bail!("expected exactly one argument");
    }

    let bytes = fs::read(&args[0])?;
    let elf = ElfFile::new(&bytes).map_err(|s| format_err!("{}", s))?;

    // wait until the USB device is available
    let mut usd = Usd::open()?;
    let usb_address = usd.usb_address();

    thread::spawn(|| {
        if let Err(e) = redirect() {
            eprintln!("serial interface error: {}", e);
        }
    });

    // do not include a DCD in the image because we'll send it over USB
    let skip_dcd = true;
    let image = Image::from_elf(&elf, skip_dcd)?;

    // cold boot: include the DCD to initialize DDR RAM
    // warm reset: omit the DCD because DDR RAM is already initialized (and trying to re-initialize
    // will hang the Armory)
    let cold_boot = env::var_os("COLD_BOOT").is_some();

    if cold_boot {
        // DCD to initialize the external DDR RAM
        let dcd = image::write::init_ddr();
        usd.dcd_write(usd::OCRAM_FREE_ADDRESS, &dcd.into_bytes())?;
    }

    let address = image.self_address();
    usd.write_file(address, &image.into_bytes())?;
    usd.jump_address(address)?;

    // the program is running now; if we see the USB device get re-enumerated it means the Armory
    // rebooted back into USD mode
    loop {
        if usd::util::has_been_reenumerated(usb_address) {
            // stop the `redirect` thread and terminate this process
            // but give it some time to flush any remaining data
            thread::sleep(Duration::from_millis(100));
            STOP.store(true, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(100));
            eprintln!("(device has reset)");
            return Ok(());
        } else {
            thread::sleep(Duration::from_millis(100))
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

/// Redirects serial data to the console
fn redirect() -> Result<(), anyhow::Error> {
    // FIXME this should look for the right port using `serialport::available_ports`
    #[cfg(target_os = "linux")]
    const PATH: &str = "/dev/ttyUSB2";
    #[cfg(not(target_os = "linux"))]
    compile_error!(
        "non-Linux host: path to serial device (debug accessory) must be entered into the program"
    );

    const BAUD_RATE: u32 = 4_000_000;
    // the FT4232H uses 512B USB packets
    const BUFSZ: usize = 512;

    let mut settings = SerialPortSettings::default();
    settings.baud_rate = BAUD_RATE;

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut buf = [0; BUFSZ];
    if let Ok(mut serial) = serialport::open_with_settings(PATH, &settings) {
        while !STOP.load(Ordering::Relaxed) {
            if serial.bytes_to_read()? != 0 {
                let len = serial.read(&mut buf)?;
                stdout.write_all(&buf[..len])?;
            } else {
                // the span of one USB micro-frame
                thread::sleep(Duration::from_micros(125));
            }
        }
    } else {
        eprintln!("warning: serial interface couldn't be opened");
    }

    Ok(())
}
