//! This is a simple test to get the source line from a child process.

mod test_utils;

#[cfg(target_os = "linux")]
use headcrab::{symbol::Dwarf, target::LinuxTarget, target::UnixTarget};

static BIN_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/testees/known_asm");

// FIXME: this should be an internal impl detail
#[cfg(target_os = "macos")]
static MAC_DSYM_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/testees/known_asm.dSYM/Contents/Resources/DWARF/known_asm"
);

// FIXME: Running this test just for linux because of privileges issue on macOS. Enable for everything after fixing.
#[cfg(target_os = "linux")]
#[test]
fn disassemble() -> Result<(), Box<dyn std::error::Error>> {
    test_utils::ensure_testees();

    #[cfg(target_os = "macos")]
    let debuginfo = Dwarf::new(MAC_DSYM_PATH)?;
    #[cfg(not(target_os = "macos"))]
    let debuginfo = Dwarf::new(BIN_PATH)?;

    let mut target = LinuxTarget::launch(BIN_PATH)?;

    // First breakpoint
    target.unpause()?;
    target.next_event()?;
    let ip = target.read_regs()?.rip;
    println!("{:08x}", ip);
    assert_eq!(debuginfo.get_address_symbol(ip as usize).as_deref(), Some("main"));

    assert_eq!(debuginfo.source_snippet(ip as usize)?, "    int $3");

    // Second breakpoint
    target.unpause()?;
    target.next_event()?;

    // Continue to exit
    target.unpause()?;

    Ok(())
}