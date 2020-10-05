use std::{convert::TryInto, ptr};

use cranelift_codegen::{binemit, entity::SecondaryMap, ir, isa::TargetIsa, Context};
use cranelift_module::{
    DataDescription, DataId, FuncId, Init, Module, ModuleCompiledFunction, ModuleDeclarations,
    ModuleError,
};
use headcrab::{target::LinuxTarget, CrabResult};
use target_lexicon::PointerWidth;

use crate::{InjectionContext, WithLinuxTarget};

#[derive(Clone)]
struct CompiledBytes {
    bytes: Vec<u8>,
    relocs: Vec<RelocEntry>,
    region: u64,
    finalized: bool,
}

// FIXME unmap memory when done
pub struct InjectionModule<'a, T: WithLinuxTarget> {
    pub(crate) inj_ctx: InjectionContext<T>,

    isa: Box<dyn TargetIsa>,
    libcall_names: Box<dyn Fn(ir::LibCall) -> String>,
    lookup_symbol: &'a dyn Fn(&str) -> u64,

    declarations: ModuleDeclarations,
    functions: SecondaryMap<FuncId, Option<CompiledBytes>>,
    data_objects: SecondaryMap<DataId, Option<CompiledBytes>>,
    functions_to_finalize: Vec<FuncId>,
    data_objects_to_finalize: Vec<DataId>,
    breakpoint_trap: u64,
}

impl<'a, T: WithLinuxTarget> InjectionModule<'a, T> {
    pub fn new(
        target: T,
        isa: Box<dyn TargetIsa>,
        lookup_symbol: &'a dyn Fn(&str) -> u64,
    ) -> CrabResult<Self> {
        let mut inj_module = Self {
            inj_ctx: InjectionContext::new(target),

            isa,
            libcall_names: cranelift_module::default_libcall_names(), // FIXME make customizable
            lookup_symbol,

            declarations: ModuleDeclarations::default(),
            functions: SecondaryMap::new(),
            data_objects: SecondaryMap::new(),
            functions_to_finalize: vec![],
            data_objects_to_finalize: vec![],
            breakpoint_trap: 0,
        };

        inj_module.breakpoint_trap = inj_module.inj_ctx.allocate_code(1, None).unwrap();
        let breakpoint_trap = inj_module.breakpoint_trap() as usize;
        inj_module.with_target(|target| target.write().write(&0xcc, breakpoint_trap).apply()).unwrap();

        Ok(inj_module)
    }

    pub fn with_target<R: Send + 'static>(&self, f: impl FnOnce(&LinuxTarget) -> R + Send) -> R {
        self.inj_ctx.with_target(f)
    }

    pub fn breakpoint_trap(&self) -> u64 {
        self.breakpoint_trap
    }

    /// Allocate a new stack and return the bottom of the stack.
    pub fn new_stack(&mut self, size: u64) -> CrabResult<u64> {
        self.inj_ctx
            .allocate_stack(size, self.breakpoint_trap() as usize)
    }

    pub fn lookup_function(&self, func_id: FuncId) -> u64 {
        let func = self.functions[func_id].as_ref().unwrap();
        assert!(func.finalized);
        func.region
    }

    pub fn lookup_data_object(&self, data_id: DataId) -> u64 {
        let data = self.data_objects[data_id].as_ref().unwrap();
        assert!(data.finalized);
        data.region
    }

    fn get_definition(&self, name: &ir::ExternalName) -> u64 {
        match *name {
            ir::ExternalName::User { .. } => {
                if self.declarations.is_function(name) {
                    let func_id = self.declarations.get_function_id(name);
                    match &self.functions[func_id] {
                        Some(compiled) => compiled.region,
                        None => {
                            (self.lookup_symbol)(&self.declarations.get_function_decl(func_id).name)
                        }
                    }
                } else {
                    let data_id = self.declarations.get_data_id(name);
                    match &self.data_objects[data_id] {
                        Some(compiled) => compiled.region,
                        None => {
                            (self.lookup_symbol)(&self.declarations.get_data_decl(data_id).name)
                        }
                    }
                }
            }
            ir::ExternalName::LibCall(ref libcall) => {
                let sym = (self.libcall_names)(*libcall);
                (self.lookup_symbol)(&sym)
            }
            _ => panic!("invalid ExternalName {}", name),
        }
    }

    fn perform_relocations(&self, bytes: &mut Vec<u8>, pos: u64, relocs: &[RelocEntry]) {
        use std::ptr::write_unaligned;

        for &RelocEntry {
            reloc,
            offset,
            ref name,
            addend,
        } in relocs
        {
            debug_assert!((offset as usize) < bytes.len());
            let ptr = bytes.as_mut_ptr();
            let at = unsafe { ptr.offset(offset as isize) };
            let base = self.get_definition(name);
            // TODO: Handle overflow.
            let what = ((base as i64) + (addend as i64)) as u64;
            match reloc {
                binemit::Reloc::Abs4 => {
                    // TODO: Handle overflow.
                    #[cfg_attr(feature = "cargo-clippy", allow(clippy::cast_ptr_alignment))]
                    unsafe {
                        write_unaligned(at as *mut u32, what as u32)
                    };
                }
                binemit::Reloc::Abs8 => {
                    #[cfg_attr(feature = "cargo-clippy", allow(clippy::cast_ptr_alignment))]
                    unsafe {
                        write_unaligned(at as *mut u64, what as u64)
                    };
                }
                binemit::Reloc::X86PCRel4 | binemit::Reloc::X86CallPCRel4 => {
                    // TODO: Handle overflow.
                    let pcrel = ((what as isize) - ((pos as isize) + (offset as isize)) /* FIXME */) as i32;
                    #[cfg_attr(feature = "cargo-clippy", allow(clippy::cast_ptr_alignment))]
                    unsafe {
                        write_unaligned(at as *mut i32, pcrel)
                    };
                }
                binemit::Reloc::X86GOTPCRel4 | binemit::Reloc::X86CallPLTRel4 => {
                    panic!("unexpected PIC relocation")
                }
                _ => unimplemented!(),
            }
        }
    }

    fn finalize_function(&mut self, func_id: FuncId) -> CrabResult<()> {
        let func = self.functions[func_id]
            .as_mut()
            .expect("function must be compiled before it can be finalized");
        assert!(!func.finalized, "function can't be finalized twice");
        func.finalized = true;
        let mut code = std::mem::take(&mut func.bytes);

        let func = self.functions[func_id].as_ref().unwrap();

        self.perform_relocations(&mut code, func.region, &func.relocs);

        self.with_target(|target| {
            target
                .write()
                .write_slice(&code, func.region as usize)
                .apply()
        })?;

        Ok(())
    }

    fn finalize_data(&mut self, data_id: DataId) -> CrabResult<()> {
        let data = self.data_objects[data_id]
            .as_mut()
            .expect("data object must be compiled before it can be finalized");
        assert!(!data.finalized, "data object can't be finalized twice");
        data.finalized = true;
        let mut bytes = std::mem::take(&mut data.bytes);

        let data = self.data_objects[data_id].as_ref().unwrap();

        self.perform_relocations(&mut bytes, data.region, &data.relocs);

        self.with_target(|target| {
            target
                .write()
                .write_slice(&bytes, data.region as usize)
                .apply()
        })?;

        Ok(())
    }

    pub fn finalize_all(&mut self) -> CrabResult<()> {
        for func_id in std::mem::take(&mut self.functions_to_finalize) {
            self.finalize_function(func_id)?;
        }

        for data_id in std::mem::take(&mut self.data_objects_to_finalize) {
            self.finalize_data(data_id)?;
        }

        Ok(())
    }
}

impl<'a, T: WithLinuxTarget> Module for InjectionModule<'a, T> {
    fn isa(&self) -> &dyn TargetIsa {
        &*self.isa
    }

    fn declarations(&self) -> &ModuleDeclarations {
        &self.declarations
    }

    fn declare_function(
        &mut self,
        name: &str,
        linkage: cranelift_module::Linkage,
        signature: &ir::Signature,
    ) -> cranelift_module::ModuleResult<FuncId> {
        let (id, _decl) = self
            .declarations
            .declare_function(name, linkage, signature)?;
        Ok(id)
    }

    fn declare_data(
        &mut self,
        name: &str,
        linkage: cranelift_module::Linkage,
        writable: bool,
        tls: bool,
    ) -> cranelift_module::ModuleResult<DataId> {
        let (id, _decl) = self
            .declarations
            .declare_data(name, linkage, writable, tls)?;
        Ok(id)
    }

    fn define_function<TS>(
        &mut self,
        func_id: FuncId,
        ctx: &mut Context,
        trap_sink: &mut TS,
    ) -> cranelift_module::ModuleResult<cranelift_module::ModuleCompiledFunction>
    where
        TS: binemit::TrapSink,
    {
        let decl = self.declarations.get_function_decl(func_id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(decl.name.clone()));
        }

        if !self.functions[func_id].is_none() {
            return Err(ModuleError::DuplicateDefinition(decl.name.to_owned()));
        }

        self.functions_to_finalize.push(func_id);
        let mut code_mem = Vec::new();
        let mut relocs = VecRelocSink::default();

        let binemit::CodeInfo { code_size, .. } = ctx
            .compile_and_emit(
                &*self.isa,
                &mut code_mem,
                &mut relocs,
                trap_sink,
                &mut binemit::NullStackMapSink {},
            )
            .unwrap();

        let code_region = self
            .inj_ctx
            .allocate_code(code_mem.len() as u64, None)
            .unwrap();

        self.functions[func_id] = Some(CompiledBytes {
            bytes: code_mem,
            relocs: relocs.0,
            region: code_region,
            finalized: false,
        });

        Ok(ModuleCompiledFunction { size: code_size })
    }

    fn define_function_bytes(
        &mut self,
        func_id: FuncId,
        bytes: &[u8],
    ) -> cranelift_module::ModuleResult<cranelift_module::ModuleCompiledFunction> {
        let decl = self.declarations.get_function_decl(func_id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(decl.name.clone()));
        }

        if !self.functions[func_id].is_none() {
            return Err(ModuleError::DuplicateDefinition(decl.name.to_owned()));
        }

        self.functions_to_finalize.push(func_id);
        let code_size = bytes.len().try_into().unwrap();

        let code_region = self
            .inj_ctx
            .allocate_code(bytes.len() as u64, None)
            .unwrap();

        self.functions[func_id] = Some(CompiledBytes {
            bytes: bytes.to_vec(),
            relocs: vec![],
            region: code_region,
            finalized: false,
        });

        Ok(ModuleCompiledFunction { size: code_size })
    }

    fn define_data(
        &mut self,
        data_id: DataId,
        data_ctx: &cranelift_module::DataContext,
    ) -> cranelift_module::ModuleResult<()> {
        let decl = self.declarations.get_data_decl(data_id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(decl.name.clone()));
        }

        if !self.data_objects[data_id].is_none() {
            return Err(ModuleError::DuplicateDefinition(decl.name.to_owned()));
        }

        assert!(!decl.tls, "InjectionModule doesn't yet support TLS");

        self.data_objects_to_finalize.push(data_id);

        let &DataDescription {
            ref init,
            ref function_decls,
            ref data_decls,
            ref function_relocs,
            ref data_relocs,
            custom_segment_section: _,
            align,
        } = data_ctx.description();

        let size = init.size();
        let data_region = if decl.writable {
            self.inj_ctx
                .allocate_readwrite(size as u64, align)
                .expect("TODO: handle OOM etc.")
        } else {
            self.inj_ctx
                .allocate_readonly(size as u64, align)
                .expect("TODO: handle OOM etc.")
        };

        let bytes = match *init {
            Init::Uninitialized => {
                panic!("data is not initialized yet");
            }
            Init::Zeros { .. } => std::iter::repeat(0).take(size).collect(),
            Init::Bytes { ref contents } => contents.clone().into_vec(),
        };

        let reloc = match self.isa.triple().pointer_width().unwrap() {
            PointerWidth::U16 => panic!(),
            PointerWidth::U32 => binemit::Reloc::Abs4,
            PointerWidth::U64 => binemit::Reloc::Abs8,
        };
        let mut relocs = Vec::new();
        for &(offset, id) in function_relocs {
            relocs.push(RelocEntry {
                reloc,
                offset,
                name: function_decls[id].clone(),
                addend: 0,
            });
        }
        for &(offset, id, addend) in data_relocs {
            relocs.push(RelocEntry {
                reloc,
                offset,
                name: data_decls[id].clone(),
                addend,
            });
        }

        self.data_objects[data_id] = Some(CompiledBytes {
            bytes,
            relocs,
            region: data_region,
            finalized: false,
        });

        Ok(())
    }
}

#[derive(Clone, Debug)]
struct RelocEntry {
    offset: binemit::CodeOffset,
    reloc: binemit::Reloc,
    name: ir::ExternalName,
    addend: binemit::Addend,
}

#[derive(Default)]
struct VecRelocSink(Vec<RelocEntry>);

impl binemit::RelocSink for VecRelocSink {
    fn reloc_block(&mut self, _: binemit::CodeOffset, _: binemit::Reloc, _: binemit::CodeOffset) {
        todo!()
    }
    fn reloc_external(
        &mut self,
        offset: binemit::CodeOffset,
        _: ir::SourceLoc,
        reloc: binemit::Reloc,
        name: &ir::ExternalName,
        addend: binemit::Addend,
    ) {
        self.0.push(RelocEntry {
            offset,
            reloc,
            name: name.clone(),
            addend,
        });
    }
    fn reloc_constant(&mut self, _: binemit::CodeOffset, _: binemit::Reloc, _: ir::ConstantOffset) {
        todo!()
    }
    fn reloc_jt(&mut self, _: binemit::CodeOffset, _: binemit::Reloc, _: ir::entities::JumpTable) {
        todo!()
    }
}
