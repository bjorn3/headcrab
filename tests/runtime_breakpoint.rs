//! This is a simple test for waiting for a fixed breakpoint in a child process.

mod test_utils;

#[cfg(target_os = "linux")]
use headcrab::{symbol::Dwarf, target::Breakpoint, target::LinuxTarget, target::UnixTarget};

static BIN_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/testees/hello");

// FIXME: this should be an internal impl detail
#[cfg(target_os = "macos")]
static MAC_DSYM_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/testees/known_asm.dSYM/Contents/Resources/DWARF/hello"
);

// FIXME: Running this test just for linux because of privileges issue on macOS. Enable for everything after fixing.
#[cfg(target_os = "linux")]
#[test]
fn fixed_breakpoint() -> Result<(), Box<dyn std::error::Error>> {
    test_utils::ensure_testees();

    #[cfg(target_os = "macos")]
    let debuginfo = Dwarf::new(MAC_DSYM_PATH)?;
    #[cfg(not(target_os = "macos"))]
    let debuginfo = Dwarf::new(BIN_PATH)?;

    let mut target = LinuxTarget::launch(BIN_PATH)?;

    let main_addr = debuginfo.get_symbol_address("main").unwrap();
    println!("{:08x}", main_addr);
    target
        .set_breakpoint(Breakpoint {
            addr: main_addr,
            on_trap: Box::new(|| {}),
        })
        .unwrap();

    // First breakpoint
    target.unpause()?;
    target.next_event()?;
    let ip = target.read_regs()?.rip;
    assert_eq!(
        debuginfo.get_address_symbol(ip as usize).as_deref(),
        Some("main")
    );
    assert_eq!(debuginfo.get_symbol_address("main"), Some(ip as usize - 1));

    // Continue to exit
    //target.unpause()?;

    Ok(())
}
