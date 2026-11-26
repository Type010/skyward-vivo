//! The functions the USB port offers, which init sets up as
//! `sys.usb.config` asks: adb while the recovery waits, with files over MTP
//! beside it while file transfer is on.

use crate::{prop, set_prop, wait_prop};
use anyhow::{anyhow, ensure, Result};
#[cfg(target_os = "android")]
use anyhow::Context;
#[cfg(target_os = "android")]
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Where a configfs gadget and adb's function live once init, or this
/// module on an init that did not make them, has set the port up.
#[cfg(target_os = "android")]
const GADGET: &str = "/config/usb_gadget/g1";
#[cfg(target_os = "android")]
const ADB_FUNCTION: &str = "/config/usb_gadget/g1/functions/ffs.adb";
#[cfg(target_os = "android")]
const ADB_LINK: &str = "/config/usb_gadget/g1/configs/b.1/f1";
#[cfg(target_os = "android")]
const ADB_FFS: &str = "/dev/usb-ffs/adb";

static MTP: AtomicBool = AtomicBool::new(false);

/// Whether the recovery makes the USB port itself, for a phone whose init
/// leaves it unset. Off, every function here does what it did before the
/// recovery learned to: nothing about the port changes.
static SELF_SETUP: AtomicBool = AtomicBool::new(false);

/// Tell the USB functions whether this recovery makes the port itself.
pub fn set_self_setup(on: bool) {
    SELF_SETUP.store(on, Ordering::Relaxed);
}

fn self_setup() -> bool {
    SELF_SETUP.load(Ordering::Relaxed)
}

/// Whether file transfer can go beside adb: its function has to be there
/// for init to bind, and a recovery whose init did not make one keeps adb
/// alone rather than losing both.
fn mtp_available() -> bool {
    Path::new("/config/usb_gadget/g1/functions/ffs.mtp").exists() && Path::new("/dev/usb-ffs/mtp").exists()
}

/// What the port offers while nothing else has it.
pub fn resting() -> &'static str {
    if !self_setup() {
        return if MTP.load(Ordering::Relaxed) { "mtp,adb" } else { "adb" };
    }
    if MTP.load(Ordering::Relaxed) && mtp_available() {
        "mtp,adb"
    } else {
        "adb"
    }
}

/// Whether MTP goes beside adb while the port rests: from when its function
/// has its descriptors until it goes.
pub fn set_mtp(on: bool) {
    MTP.store(on, Ordering::Relaxed);
}

/// Ask init for `config` and wait until it is in place. Going to "none"
/// also waits for adbd to be gone, so its function is free.
pub fn set(config: &str, timeout: Duration) -> Result<()> {
    set_prop("sys.usb.config", config)?;
    wait_prop("sys.usb.state", config, timeout)?;
    if config == "none" {
        let start = Instant::now();
        while !matches!(prop("init.svc.adbd").as_deref(), None | Some("stopped")) {
            ensure!(start.elapsed() < timeout, "adbd still runs {} ms after the port went to none", timeout.as_millis());
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

/// Hand the port to `config` through "none", which stops the programs
/// serving it and takes every function off; init then sets the port up
/// once the programs behind the new configuration are ready.
pub fn switch(config: &str, timeout: Duration) -> Result<()> {
    set("none", timeout)?;
    if !self_setup() {
        return set(config, timeout);
    }
    match set(config, timeout) {
        Ok(()) => Ok(()),
        Err(e) => {
            // A function that did not come up must not leave the port with
            // none: adb is what the recovery can always serve.
            let _ = start_adb(timeout);
            Err(e)
        }
    }
}

/// Whether the port is serving adb, not merely named as such: init runs
/// adbd and adbd has opened its function. A configuration that stopped
/// after naming adb leaves the state behind with nothing behind it.
pub fn adb_ready() -> bool {
    prop("sys.usb.state").as_deref() == Some("adb")
        && prop("init.svc.adbd").as_deref() == Some("running")
        && prop("sys.usb.ffs.ready").as_deref() == Some("1")
}

/// Bring adb up, taking the port through "none" first when it is not
/// serving adb. The first step is best effort: adb is what is wanted, and
/// asking for it with the function free matters more than init answering
/// the step before in time.
pub fn start_adb(timeout: Duration) -> Result<()> {
    if !self_setup() {
        // What the recovery always did: take a function of another kind
        // off the port, then ask init for adb.
        if prop("sys.usb.state").is_some_and(|s| s != "adb") {
            set("none", timeout)?;
        }
        return set_prop("sys.usb.config", "adb");
    }
    // An init that did not run its USB rules leaves no gadget for adbd to
    // open and none for the port to be bound through. Make both here; a
    // failure is told, and the rest of the start still gets its turn.
    #[cfg(target_os = "android")]
    if let Err(e) = prepare_adb_port() {
        let _ = crate::kmsg(&format!("adb port not prepared: {e:#}"));
    }
    if adb_ready() {
        return Ok(());
    }
    // An adbd already on its way is given a moment to open its function
    // before the port is taken from it.
    if prop("init.svc.adbd").as_deref() == Some("running") {
        let _ = wait_prop("sys.usb.ffs.ready", "1", timeout.min(Duration::from_secs(1)));
        if adb_ready() {
            return Ok(());
        }
    }
    // The controller init names from the bootloader is what the port is
    // bound to; an init that did not run its own rule would leave it unset.
    if prop("sys.usb.controller").is_none()
        && let Some(controller) = prop("ro.boot.usbcontroller")
    {
        let _ = set_prop("sys.usb.controller", controller.as_str());
    }
    let _ = set("none", timeout);
    if set("adb", timeout).is_ok() {
        return Ok(());
    }
    // The property actions may not have run: taking the port through
    // "none" took the function's link and state off, and nothing has put
    // them back. Put them back, ask init for adbd itself, and bind the
    // port here once adbd has opened its function.
    #[cfg(target_os = "android")]
    {
        let _ = prepare_gadget();
        let _ = set_prop("ctl.start", "adbd");
        let _ = wait_prop("sys.usb.ffs.ready", "1", timeout);
        if prop("sys.usb.ffs.ready").as_deref() == Some("1")
            && let Some(controller) = prop("sys.usb.controller")
        {
            let _ = fs::write(format!("{GADGET}/UDC"), &controller);
            let _ = set_prop("sys.usb.state", "adb");
        }
    }
    if adb_ready() {
        return Ok(());
    }
    Err(anyhow!(
        "adbd is {:?}, ffs ready {:?}, usb state {:?}, controller {:?}",
        prop("init.svc.adbd"),
        prop("sys.usb.ffs.ready"),
        prop("sys.usb.state"),
        prop("sys.usb.controller")
    ))
}

/// Make the function adbd opens and the configfs gadget init binds,
/// where the recovery's init left either out.
#[cfg(target_os = "android")]
fn prepare_adb_port() -> Result<()> {
    prepare_functionfs()?;
    if !Path::new(GADGET).join("UDC").exists() {
        prepare_configfs()?;
        prepare_gadget()?;
    }
    // The rules that put the function on the port ask for configfs; an
    // init that did not set it never runs them.
    let _ = set_prop("sys.usb.configfs", "1");
    Ok(())
}

/// Mount adbd's functionfs where it opens it, unless init already did.
#[cfg(target_os = "android")]
fn prepare_functionfs() -> Result<()> {
    if skyward_block::mount::is_mount_point(Path::new(ADB_FFS))? {
        return Ok(());
    }
    fs::create_dir_all(ADB_FFS).with_context(|| format!("make {ADB_FFS}"))?;
    match skyward_block::mount::mount(Path::new("adb"), Path::new(ADB_FFS), "functionfs", &["uid=2000".into(), "gid=2000".into()]) {
        Ok(()) => Ok(()),
        // It is mounted, only the table this boot keeps did not say so.
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.raw_os_error() == Some(libc::EBUSY)) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("mount functionfs on {ADB_FFS}")),
    }
}

/// Mount configfs where the gadget lives, unless it is already mounted.
#[cfg(target_os = "android")]
fn prepare_configfs() -> Result<()> {
    fs::create_dir_all("/config").with_context(|| "make /config")?;
    // Busy means it is mounted already, which is what is wanted.
    let _ = skyward_block::mount::mount(Path::new("configfs"), Path::new("/config"), "configfs", &[]);
    Ok(())
}

/// Make the gadget init's own rules would: one adb function, the
/// identifiers a recovery advertises, and the link that puts the function
/// on the port.
#[cfg(target_os = "android")]
fn prepare_gadget() -> Result<()> {
    fs::create_dir_all(format!("{GADGET}/strings/0x409"))?;
    fs::write(format!("{GADGET}/idVendor"), "0x18D1")?;
    fs::write(format!("{GADGET}/idProduct"), "0xD001")?;
    fs::write(format!("{GADGET}/strings/0x409/manufacturer"), "vivo")?;
    fs::write(format!("{GADGET}/strings/0x409/product"), "vivo")?;
    fs::write(format!("{GADGET}/strings/0x409/serialnumber"), prop("ro.serialno").unwrap_or_default())?;
    fs::create_dir_all(ADB_FUNCTION)?;
    fs::create_dir_all(format!("{GADGET}/configs/b.1/strings/0x409"))?;
    fs::write(format!("{GADGET}/configs/b.1/MaxPower"), "0x384")?;
    fs::write(format!("{GADGET}/configs/b.1/strings/0x409/configuration"), "adb")?;
    match fs::read_link(ADB_LINK) {
        Ok(target) if target == Path::new(ADB_FUNCTION) => {}
        _ => {
            let _ = fs::remove_file(ADB_LINK);
            std::os::unix::fs::symlink(ADB_FUNCTION, ADB_LINK)?;
        }
    }
    Ok(())
}
