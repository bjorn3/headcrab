// FIXME make this work on other systems too.
#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use cranelift_codegen::{
    isa::{self, TargetIsa},
    settings::{self, Configurable},
};

use headcrab::{target::LinuxTarget, CrabResult};

mod memory;
mod module;
mod old_module;
mod worker_thread;

pub use cranelift_codegen::Context;
pub use cranelift_module::{DataId, FuncId, FuncOrDataId};
pub use cranelift_reader::parse_functions;
pub use memory::Memory;
pub use module::InjectionModule;
pub use old_module::OldInjectionModule;
pub use worker_thread::WorkerThread;

const EXECUTABLE_DATA_ALIGNMENT: u64 = 0x10;
const WRITABLE_DATA_ALIGNMENT: u64 = 0x8;
const READONLY_DATA_ALIGNMENT: u64 = 0x1;

pub fn target_isa() -> Box<dyn TargetIsa> {
    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").unwrap();
    let flags = settings::Flags::new(flag_builder);
    isa::lookup("x86_64".parse().unwrap())
        .unwrap()
        .finish(flags)
}

fn parse_func_or_data(s: &str) -> FuncOrDataId {
    let (kind, index) = s.split_at(4);
    let index: u32 = index.parse().unwrap();

    match kind {
        "func" => FuncOrDataId::Func(FuncId::from_u32(index)),
        "data" => FuncOrDataId::Data(DataId::from_u32(index)),
        _ => panic!("`Unknown kind {}`", kind),
    }
}

pub fn inject_clif_code(
    inj_module: &mut OldInjectionModule,
    lookup_symbol: &dyn Fn(&str) -> u64,
    code: &str,
) -> CrabResult<u64> {
    let mut run_function = None;

    for line in code.lines() {
        let line = line.trim();
        if !line.starts_with(';') {
            continue;
        }
        let line = line.trim_start_matches(';').trim_start();
        let (directive, content) = line.split_at(line.find(':').unwrap_or(line.len()));
        let content = content[1..].trim_start();

        match directive {
            "declare" => {
                let (id, content) = content.split_at(content.find(" ").unwrap_or(content.len()));
                let content = content.trim_start();
                match parse_func_or_data(id) {
                    FuncOrDataId::Func(func_id) => {
                        inj_module.define_function(func_id, lookup_symbol(content));
                    }
                    FuncOrDataId::Data(data_id) => {
                        inj_module.define_data_object(data_id, lookup_symbol(content));
                    }
                }
            }
            "define" => {
                let (id, content) = content.split_at(content.find(" ").unwrap_or(content.len()));
                let content = content.trim_start();
                match parse_func_or_data(id) {
                    FuncOrDataId::Data(data_id) => {
                        if content.starts_with('"') {
                            let content = content
                                .trim_matches('"')
                                .replace("\\n", "\n")
                                .replace("\\0", "\0");
                            inj_module
                                .define_data_object_with_bytes(data_id, content.as_bytes())?;
                        } else {
                            todo!();
                        }
                    }
                    FuncOrDataId::Func(func_id) => {
                        panic!("Please use `function u0:{}()` instead", func_id.as_u32());
                    }
                }
            }
            "run" => {
                assert!(run_function.is_none());
                match parse_func_or_data(content) {
                    FuncOrDataId::Func(func_id) => run_function = Some(func_id),
                    FuncOrDataId::Data(_) => panic!("Can't execute data object"),
                }
            }
            _ => panic!("Unknown directive `{}`", directive),
        }
    }

    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").unwrap();
    let flags = settings::Flags::new(flag_builder);
    let isa = isa::lookup("x86_64".parse().unwrap())
        .unwrap()
        .finish(flags);

    let functions = cranelift_reader::parse_functions(code).unwrap();
    let mut ctx = cranelift_codegen::Context::new();
    for func in functions {
        ctx.clear();
        ctx.func = func;
        inj_module.compile_clif_code(&*isa, &mut ctx)?;
    }

    let run_function = inj_module.lookup_function(run_function.expect("Missing `run` directive"));

    Ok(run_function)
}

pub struct InjectionContext<'a> {
    target: WorkerThread<LinuxTarget>,
    code: Memory,
    readonly: Memory,
    readwrite: Memory,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> InjectionContext<'a> {
    pub fn new(target: WorkerThread<LinuxTarget>) -> Self {
        Self {
            target,
            code: Memory::new_executable(),
            readonly: Memory::new_readonly(),
            readwrite: Memory::new_writable(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn with_target<R: Send + 'static>(&self, f: impl FnOnce(&LinuxTarget) -> R + Send) -> R {
        self.target.spawn(move |d| f(d))
    }

    pub fn allocate_code(&mut self, size: u64, align: Option<u64>) -> CrabResult<u64> {
        let code = &mut self.code;
        self.target.spawn(move |target| {
            code.allocate(target, size, align.unwrap_or(EXECUTABLE_DATA_ALIGNMENT))
        })
    }

    pub fn allocate_readonly(&mut self, size: u64, align: Option<u64>) -> CrabResult<u64> {
        let readonly = &mut self.readonly;
        self.target.spawn(move |target| {
            readonly.allocate(target, size, align.unwrap_or(READONLY_DATA_ALIGNMENT))
        })
    }

    pub fn allocate_readwrite(&mut self, size: u64, align: Option<u64>) -> CrabResult<u64> {
        let readwrite = &mut self.readwrite;
        self.target.spawn(move |target| {
            readwrite.allocate(target, size, align.unwrap_or(WRITABLE_DATA_ALIGNMENT))
        })
    }

    /// Allocate a new stack and return the bottom of the stack.
    pub fn allocate_stack(&mut self, size: u64, return_addr: usize) -> CrabResult<u64> {
        let readwrite = &mut self.readwrite;

        self.target.spawn(|target| {
            let stack = readwrite.allocate(target, size, 16)?;

            target
                .write()
                .write(
                    &return_addr,
                    stack as usize + size as usize - std::mem::size_of::<usize>(),
                )
                .apply()?;

            // Stack grows downwards on x86_64
            Ok(stack + size - std::mem::size_of::<usize>() as u64)
        })
    }

    pub fn write(&mut self, data: &[u8], ptr: usize) -> CrabResult<()> {
        self.target
            .spawn(move |target| target.write().write_slice(data, ptr).apply())
    }
}
