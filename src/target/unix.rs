use nix::{
    sys::ptrace,
    sys::wait::waitpid,
    unistd::{execv, fork, ForkResult, Pid},
};
use std::ffi::CString;
use std::process;

/// This trait defines the common behavior for all *nix targets
pub trait UnixTarget {
    /// Provides the Pid of the debugee process
    fn pid(&self) -> Pid;

    /// Continues execution of a debuggee.
    fn unpause(&self) -> Result<(), Box<dyn std::error::Error>> {
        ptrace::cont(self.pid(), None)?;
        Ok(())
    }
}

/// Launch a new debuggee process.
pub(crate) fn launch(path: CString) -> Result<Pid, Box<dyn std::error::Error>> {
    // We start the debuggee by forking the parent process.
    // The child process invokes `ptrace(2)` with the `PTRACE_TRACEME` parameter to enable debugging features for the parent.
    // This requires a user to have a `SYS_CAP_PTRACE` permission. See `man capabilities(7)` for more information.
    match fork()? {
        ForkResult::Parent { child, .. } => {
            let _status = waitpid(child, None);

            // todo: handle this properly
            Ok(child)
        }
        ForkResult::Child => {
            if let Err(err) = ptrace::traceme() {
                println!("ptrace traceme failed: {:?}", err);
                process::abort()
            }

            if let Err(err) = execv(&path, &[]) {
                println!("execv failed: {:?}", err);
                process::abort();
            }

            // execv replaces the process image, so this place in code will not be reached.
            println!("Unreachable code reached");
            process::abort();
        }
    }
}
