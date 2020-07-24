// This module provides a naive implementation of symbolication for the time being.
// It should be expanded to support multiple data sources.

use gimli::read::{EvaluationResult, Reader as _};
use object::read::{Object, ObjectSection};
use std::{borrow::Cow, collections::BTreeMap, fs::File, rc::Rc};

macro_rules! dwarf_attr_or_continue {
    (str($dwarf:ident,$unit:ident) $entry:ident.$name:ident) => {
        $dwarf
            .attr_string(&$unit, dwarf_attr_or_continue!($entry.$name).value())?
            .to_string()?;
    };
    ($entry:ident.$name:ident) => {
        if let Some(attr) = $entry.attr(gimli::$name)? {
            attr
        } else {
            continue;
        }
    };
}

rental! {
    mod inner {
        use super::*;

        #[rental]
        pub(super) struct DwarfInner {
            mmap: Box<memmap::Mmap>,
            parsed: ParsedDwarf<'mmap>,
        }
    }
}

#[derive(Debug)]
enum RcCow<'a, T: ?Sized> {
    Owned(Rc<T>),
    Borrowed(&'a T),
}

impl<T: ?Sized> Clone for RcCow<'_, T> {
    fn clone(&self) -> Self {
        match self {
            RcCow::Owned(rc) => RcCow::Owned(rc.clone()),
            RcCow::Borrowed(slice) => RcCow::Borrowed(&**slice),
        }
    }
}

impl<T: ?Sized> std::ops::Deref for RcCow<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            RcCow::Owned(rc) => &**rc,
            RcCow::Borrowed(slice) => &**slice,
        }
    }
}

unsafe impl<T: ?Sized> gimli::StableDeref for RcCow<'_, T> {}
unsafe impl<T: ?Sized> gimli::CloneStableDeref for RcCow<'_, T> {}

type Reader<'a> = gimli::EndianReader<gimli::RunTimeEndian, RcCow<'a, [u8]>>;

struct ParsedDwarf<'mmap> {
    object: object::File<'mmap>,
    dwarf: gimli::Dwarf<Reader<'mmap>>,
    vars: BTreeMap<String, usize>,
}

pub struct Dwarf {
    inner: inner::DwarfInner,
}

impl Dwarf {
    // todo: impl loader struct instead of taking 'path' as an argument.
    // It will be required to e.g. load coredumps, or external debug info, or to
    // communicate with rustc/lang servers.
    pub fn new(path: &str) -> Result<Dwarf, Box<dyn std::error::Error>> {
        // This is completely inefficient and hacky code, but currently it serves the only
        // purpose of getting addresses of static variables.
        // TODO: this will be reworked in a more complete symbolication framework.
        let mut vars = BTreeMap::new();

        // Load ELF/Mach-O object file
        let file = File::open(path)?;
        let mmap = unsafe { memmap::Mmap::map(&file)? };

        let inner = inner::DwarfInner::try_new(Box::new(mmap), |mmap| {
            // FIXME extract to function on ParsedDwarf
            let object = object::File::parse(&*mmap)?;

            let endian = if object.is_little_endian() {
                gimli::RunTimeEndian::Little
            } else {
                gimli::RunTimeEndian::Big
            };

            // This can be also processed in parallel.
            let loader = |id: gimli::SectionId| -> Result<Reader, gimli::Error> {
                match object.section_by_name(id.name()) {
                    Some(ref section) => {
                        let data = section
                            .uncompressed_data()
                            .unwrap_or(Cow::Borrowed(&[][..]));
                        let data = match data {
                            Cow::Owned(vec) => RcCow::Owned(vec.into()),
                            Cow::Borrowed(slice) => RcCow::Borrowed(slice),
                        };
                        Ok(gimli::EndianReader::new(data, endian))
                    }
                    None => Ok(gimli::EndianReader::new(RcCow::Borrowed(&[][..]), endian)),
                }
            };
            // we don't support supplementary object files for now
            let sup_loader = |_| Ok(gimli::EndianReader::new(RcCow::Borrowed(&[][..]), endian));

            // Create `EndianSlice`s for all of the sections.
            let dwarf = gimli::Dwarf::load(loader, sup_loader)?;

            let mut units = dwarf.units();

            while let Some(header) = units.next()? {
                let unit = dwarf.unit(header)?;
                let mut entries = unit.entries();
                while let Some((_, entry)) = entries.next_dfs()? {
                    if entry.tag() == gimli::DW_TAG_variable {
                        let name =
                            dwarf_attr_or_continue!(str(dwarf, unit) entry.DW_AT_name).into_owned();
                        let expr = dwarf_attr_or_continue!(entry.DW_AT_location).exprloc_value();

                        // TODO: evaluation should not happen here
                        if let Some(expr) = expr {
                            let mut eval = expr.evaluation(unit.encoding());
                            match eval.evaluate()? {
                                EvaluationResult::RequiresRelocatedAddress(reloc_addr) => {
                                    vars.insert(name.to_owned(), reloc_addr as usize);
                                }
                                _ev_res => {} // do nothing for now
                            }
                        }
                    }
                }
            }

            Ok(ParsedDwarf {
                object,
                dwarf,
                vars,
            })
        })
        .map_err(|err: rental::RentalError<Box<dyn std::error::Error>, _>| err.0)?;

        Ok(Dwarf { inner })
    }

    pub fn get_var_address(&self, name: &str) -> Option<usize> {
        self.inner.rent(|parsed| parsed.vars.get(name).cloned())
    }
}
