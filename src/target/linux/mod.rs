mod readmem;
mod writemem;

use nix::unistd::{getpid, Pid};
use std::{
    collections::HashMap,
    ffi::CString,
    fs::File,
    io::{BufRead, BufReader},
};

use crate::target::unix::{self, UnixTarget};
pub use readmem::ReadMemory;
pub use writemem::WriteMemory;

/// This structure holds the state of a debuggee on Linux based systems
/// You can use it to read & write debuggee's memory, pause it, set breakpoints, etc.
pub struct LinuxTarget {
    pid: Pid,
    breakpoints: HashMap<usize, BreakpointEntry>,
}

struct BreakpointEntry {
    replaced_byte: u8,
    on_trap: Box<dyn FnMut()>,
}

pub struct Breakpoint {
    pub addr: usize,
    pub on_trap: Box<dyn FnMut()>,
}

impl UnixTarget for LinuxTarget {
    /// Provides the Pid of the debugee process
    fn pid(&self) -> Pid {
        self.pid
    }
}

impl LinuxTarget {
    /// Launches a new debuggee process
    pub fn launch(path: &str) -> Result<LinuxTarget, Box<dyn std::error::Error>> {
        let pid = unix::launch(CString::new(path)?)?;
        Ok(LinuxTarget {
            pid,
            breakpoints: HashMap::new(),
        })
    }

    /// Attaches process as a debugee.
    pub fn attach(pid: Pid) -> Result<LinuxTarget, Box<dyn std::error::Error>> {
        unix::attach(pid)?;
        Ok(LinuxTarget {
            pid,
            breakpoints: HashMap::new(),
        })
    }

    /// Uses this process as a debuggee.
    pub fn me() -> LinuxTarget {
        LinuxTarget {
            pid: getpid(),
            breakpoints: HashMap::new(),
        }
    }

    /// Reads memory from a debuggee process.
    pub fn read(&self) -> ReadMemory {
        ReadMemory::new(self.pid())
    }

    /// Writes memory to a debuggee process.
    pub fn write(&self) -> WriteMemory {
        WriteMemory::new(self.pid())
    }

    /// Reads the register values from the main thread of a debuggee process.
    pub fn read_regs(&self) -> Result<libc::user_regs_struct, Box<dyn std::error::Error>> {
        nix::sys::ptrace::getregs(self.pid()).map_err(|err| err.into())
    }

    pub fn set_breakpoint(
        &mut self,
        breakpoint: Breakpoint,
    ) -> Result<(), Box<dyn std::error::Error>> {
        const INT3: libc::c_long = 0xcc;
        let word = nix::sys::ptrace::read(self.pid(), breakpoint.addr as *mut _)?;
        assert!(
            self.breakpoints
                .insert(
                    breakpoint.addr,
                    BreakpointEntry {
                        replaced_byte: word as u8,
                        on_trap: breakpoint.on_trap
                    }
                )
                .is_none(),
            "Breakpoint already set"
        );
        let word = (word & !0xff) | INT3;
        nix::sys::ptrace::write(self.pid(), breakpoint.addr as *mut _, word as *mut _)?;
        Ok(())
    }

    pub fn remove_breakpoint(
        &mut self,
        addr: usize,
    ) -> Result<Breakpoint, Box<dyn std::error::Error>> {
        let breakpoint_entry = self
            .breakpoints
            .remove(&addr)
            .ok_or_else(|| "Breakpoint not found".to_string())?;
        let word = nix::sys::ptrace::read(self.pid(), addr as *mut _)?;
        let word = (word & !0xff) | breakpoint_entry.replaced_byte as libc::c_long;
        nix::sys::ptrace::write(self.pid(), addr as *mut _, word as *mut _)?;
        Ok(Breakpoint {
            addr,
            on_trap: breakpoint_entry.on_trap,
        })
    }

    /// Continues execution of a debuggee.
    pub fn unpause(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut regs = self.read_regs()?;

        if self.breakpoints.get(&(regs.rip as usize - 1)).is_some() {
            let breakpoint = self.remove_breakpoint(regs.rip as usize - 1)?;
            nix::sys::ptrace::step(self.pid(), None)?;
            nix::sys::wait::waitpid(self.pid(), None)?;

            // Set replaced byte back to the original instruction and move instruction pointer back.
            self.set_breakpoint(Breakpoint {
                addr: regs.rip as usize - 1,
                on_trap: breakpoint.on_trap,
            })?;
            regs.rip -= 1;
            nix::sys::ptrace::setregs(self.pid(), regs)?;
        }
        nix::sys::ptrace::cont(self.pid(), None)?;
        Ok(())
    }

    /// Waits for the next debug event.
    // FIXME return the actual event
    pub fn next_event(&self) -> Result<(), Box<dyn std::error::Error>> {
        nix::sys::wait::waitpid(self.pid(), None)?;
        Ok(())
    }
}

/// Returns the start of a process's virtual memory address range.
/// This can be useful for calculation of relative addresses in memory.
pub fn get_addr_range(pid: Pid) -> Result<usize, Box<dyn std::error::Error>> {
    let file = File::open(format!("/proc/{}/maps", pid))?;
    let mut bufread = BufReader::new(file);
    let mut proc_map = String::new();

    bufread.read_line(&mut proc_map)?;

    let proc_data: Vec<_> = proc_map.split(' ').collect();
    let addr_range: Vec<_> = proc_data[0].split('-').collect();

    Ok(usize::from_str_radix(addr_range[0], 16)?)
}

#[cfg(test)]
mod tests {
    use super::ReadMemory;
    use nix::unistd::getpid;

    use std::alloc::{alloc_zeroed, dealloc, Layout};

    use nix::sys::mman::{mprotect, ProtFlags};

    #[test]
    fn read_memory() {
        let var: usize = 52;
        let var2: u8 = 128;

        let mut read_var_op: usize = 0;
        let mut read_var2_op: u8 = 0;

        unsafe {
            ReadMemory::new(getpid())
                .read(&mut read_var_op, &var as *const _ as usize)
                .read(&mut read_var2_op, &var2 as *const _ as usize)
                .apply()
                .expect("Failed to apply memop");
        }

        assert_eq!(read_var2_op, var2);
        assert_eq!(read_var_op, var);
    }

    const PAGE_SIZE: usize = 4096;

    #[test]
    fn read_protected_memory() {
        let mut read_var_op: usize = 0;

        unsafe {
            let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();
            let ptr = alloc_zeroed(layout);

            *(ptr as *mut usize) = 9921;

            mprotect(
                ptr as *mut std::ffi::c_void,
                PAGE_SIZE,
                ProtFlags::PROT_NONE,
            )
            .expect("Failed to mprotect");

            let res = ReadMemory::new(getpid())
                .read(&mut read_var_op, ptr as *const _ as usize)
                .apply();

            // Expected to fail when reading read-protected memory.
            // FIXME: Change when reading read-protected memory is handled properly
            match res {
                Ok(()) => panic!("Unexpected result: reading protected memory succeeded"),
                Err(_) => (),
            }

            mprotect(
                ptr as *mut std::ffi::c_void,
                PAGE_SIZE,
                ProtFlags::PROT_WRITE,
            )
            .expect("Failed to mprotect");
            dealloc(ptr, layout);
        }
    }

    #[test]
    fn read_cross_page_memory() {
        let mut read_var_op = [0u32; 2];

        unsafe {
            let layout = Layout::from_size_align(PAGE_SIZE * 2, PAGE_SIZE).unwrap();
            let ptr = alloc_zeroed(layout);

            let array_ptr = (ptr as usize + PAGE_SIZE - std::mem::size_of::<u32>()) as *mut u8;
            *(array_ptr as *mut [u32; 2]) = [123, 456];

            let second_page_ptr = (ptr as usize + PAGE_SIZE) as *mut std::ffi::c_void;

            mprotect(second_page_ptr, PAGE_SIZE, ProtFlags::PROT_NONE).expect("Failed to mprotect");

            ReadMemory::new(getpid())
                .read(&mut read_var_op, array_ptr as *const _ as usize)
                .apply()
                .expect("Failed to apply memop");

            // Expected result because of cross page read
            // FIXME: Change when cross page read is handled correctly
            assert_eq!([123, 0], read_var_op);

            mprotect(second_page_ptr, PAGE_SIZE, ProtFlags::PROT_WRITE)
                .expect("Failed to mprotect");
            dealloc(ptr, layout);
        }
    }
}
