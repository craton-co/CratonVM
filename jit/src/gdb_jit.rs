// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The GDB/LLDB JIT compilation interface (`CRATONVM_JIT_GDB`, default-OFF,
//! Linux only).
//!
//! A debugger that sees the symbols `__jit_debug_register_code` and
//! `__jit_debug_descriptor` in the inferior sets a breakpoint on the function
//! and, each time it is hit, reads `descriptor.action_flag` and
//! `descriptor.relevant_entry`, then loads (or drops) the in-memory object file
//! that entry points at. GDB documents the protocol under "JIT Compilation
//! Interface"; LLDB implements the same one (`settings set
//! plugin.jit-loader.gdb.enable on`).
//!
//! Each published region gets a minimal ELF64 relocatable object: a `.text`
//! section of type `SHT_NOBITS` whose `sh_addr` is the code's runtime address
//! and whose size is the code size, one global `STT_FUNC` symbol at offset 0 of
//! that section named after the method, and the `.strtab`/`.shstrtab` it
//! needs. There is no line table and no unwind data: the debugger can name a
//! JIT frame and set a breakpoint on it, and it unwinds through it by the
//! frame-pointer chain.
//!
//! Entries stay registered until [`unregister`] runs from
//! `ExecutableBuffer::drop`, just before the mapping is returned, so a
//! debugger never holds a symbol for an address that has been reused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

pub const JIT_NOACTION: u32 = 0;
pub const JIT_REGISTER_FN: u32 = 1;
pub const JIT_UNREGISTER_FN: u32 = 2;

// ---------------------------------------------------------------------------
// ELF builder (pure; built and tested on every host)
// ---------------------------------------------------------------------------

pub const ELF_HEADER_SIZE: usize = 64;
pub const SECTION_HEADER_SIZE: usize = 64;
pub const SYMBOL_SIZE: usize = 24;
/// null, `.text`, `.symtab`, `.strtab`, `.shstrtab`.
pub const SECTION_COUNT: usize = 5;
pub const TEXT_SECTION: u16 = 1;
pub const SYMTAB_SECTION: u16 = 2;
pub const STRTAB_SECTION: u16 = 3;
pub const SHSTRTAB_SECTION: u16 = 4;

pub const SHT_PROGBITS: u32 = 1;
pub const SHT_SYMTAB: u32 = 2;
pub const SHT_STRTAB: u32 = 3;
pub const SHT_NOBITS: u32 = 8;
const SHF_ALLOC: u64 = 0x2;
const SHF_EXECINSTR: u64 = 0x4;
const ET_REL: u16 = 1;
const STB_GLOBAL: u8 = 1;
const STT_FUNC: u8 = 2;

/// The ELF machine for this host (`EM_X86_64`, `EM_AARCH64`, or 0).
fn elf_machine() -> u16 {
    // Cast: both machine numbers fit in the 16-bit `e_machine` field.
    crate::jitdump::host_elf_machine() as u16
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[allow(clippy::too_many_arguments)]
fn put_section_header(
    out: &mut Vec<u8>,
    name: u32,
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
) {
    put_u32(out, name);
    put_u32(out, kind);
    put_u64(out, flags);
    put_u64(out, addr);
    put_u64(out, offset);
    put_u64(out, size);
    put_u32(out, link);
    put_u32(out, info);
    put_u64(out, align);
    put_u64(out, entsize);
}

fn pad_to(out: &mut Vec<u8>, align: usize) {
    while out.len() % align != 0 {
        out.push(0);
    }
}

/// Build the in-memory object file describing `[start, start+size)` as one
/// function called `name`. Little-endian ELF64 (both supported hosts are
/// little-endian). NUL bytes in `name` are dropped.
pub fn build_elf(start: u64, size: u64, name: &str) -> Vec<u8> {
    // .shstrtab
    let mut shstrtab = vec![0u8];
    let mut add_name = |s: &str| -> u32 {
        let at = shstrtab.len() as u32;
        shstrtab.extend_from_slice(s.as_bytes());
        shstrtab.push(0);
        at
    };
    let text_name = add_name(".text");
    let symtab_name = add_name(".symtab");
    let strtab_name = add_name(".strtab");
    let shstrtab_name = add_name(".shstrtab");

    // .strtab: "\0<name>\0"
    let mut strtab = vec![0u8];
    strtab.extend(name.bytes().filter(|&b| b != 0));
    strtab.push(0);

    // .symtab: the null symbol, then the function.
    let mut symtab = Vec::with_capacity(2 * SYMBOL_SIZE);
    symtab.extend_from_slice(&[0u8; SYMBOL_SIZE]);
    put_u32(&mut symtab, 1); // st_name
    symtab.push((STB_GLOBAL << 4) | STT_FUNC); // st_info
    symtab.push(0); // st_other: STV_DEFAULT
    put_u16(&mut symtab, TEXT_SECTION); // st_shndx
    put_u64(&mut symtab, 0); // st_value: offset 0 in .text, whose sh_addr is `start`
    put_u64(&mut symtab, size); // st_size

    let mut out = Vec::with_capacity(
        ELF_HEADER_SIZE + shstrtab.len() + strtab.len() + symtab.len() + 16
            + SECTION_COUNT * SECTION_HEADER_SIZE,
    );
    // Header; e_shoff is patched once the section data is laid out.
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F']);
    out.push(2); // ELFCLASS64
    out.push(1); // ELFDATA2LSB
    out.push(1); // EV_CURRENT
    out.push(0); // ELFOSABI_NONE
    out.extend_from_slice(&[0u8; 8]); // ABI version + padding
    put_u16(&mut out, ET_REL);
    put_u16(&mut out, elf_machine());
    put_u32(&mut out, 1); // e_version
    put_u64(&mut out, 0); // e_entry
    put_u64(&mut out, 0); // e_phoff
    let shoff_at = out.len();
    put_u64(&mut out, 0); // e_shoff, patched
    put_u32(&mut out, 0); // e_flags
    put_u16(&mut out, ELF_HEADER_SIZE as u16);
    put_u16(&mut out, 0); // e_phentsize
    put_u16(&mut out, 0); // e_phnum
    put_u16(&mut out, SECTION_HEADER_SIZE as u16);
    put_u16(&mut out, SECTION_COUNT as u16);
    put_u16(&mut out, SHSTRTAB_SECTION);
    debug_assert_eq!(out.len(), ELF_HEADER_SIZE);

    let shstrtab_off = out.len();
    out.extend_from_slice(&shstrtab);
    let strtab_off = out.len();
    out.extend_from_slice(&strtab);
    pad_to(&mut out, 8);
    let symtab_off = out.len();
    out.extend_from_slice(&symtab);
    pad_to(&mut out, 8);
    let shoff = out.len();
    out[shoff_at..shoff_at + 8].copy_from_slice(&(shoff as u64).to_le_bytes());

    // [0] null
    out.extend_from_slice(&[0u8; SECTION_HEADER_SIZE]);
    // [1] .text — occupies no bytes in the file; it describes the live code.
    put_section_header(
        &mut out,
        text_name,
        SHT_NOBITS,
        SHF_ALLOC | SHF_EXECINSTR,
        start,
        ELF_HEADER_SIZE as u64,
        size,
        0,
        0,
        1,
        0,
    );
    // [2] .symtab — sh_info is the index of the first non-local symbol.
    put_section_header(
        &mut out,
        symtab_name,
        SHT_SYMTAB,
        0,
        0,
        symtab_off as u64,
        symtab.len() as u64,
        u32::from(STRTAB_SECTION),
        1,
        8,
        SYMBOL_SIZE as u64,
    );
    // [3] .strtab
    put_section_header(
        &mut out,
        strtab_name,
        SHT_STRTAB,
        0,
        0,
        strtab_off as u64,
        strtab.len() as u64,
        0,
        0,
        1,
        0,
    );
    // [4] .shstrtab
    put_section_header(
        &mut out,
        shstrtab_name,
        SHT_STRTAB,
        0,
        0,
        shstrtab_off as u64,
        shstrtab.len() as u64,
        0,
        0,
        1,
        0,
    );
    out
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

static FAILED: AtomicBool = AtomicBool::new(false);
/// Set on the first registration, so [`unregister`] (which runs on every
/// buffer drop) can return without reading a flag or taking a lock.
static EVER_REGISTERED: AtomicBool = AtomicBool::new(false);

/// `CRATONVM_JIT_GDB`, read once.
fn requested() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::code_events::flag_is_on(cratonvm_types::flags::runtime_var("CRATONVM_JIT_GDB"))
    })
}

/// Whether published regions should be registered with a debugger.
pub fn enabled() -> bool {
    if !requested() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        true
    }
    #[cfg(not(target_os = "linux"))]
    {
        if !FAILED.swap(true, Ordering::SeqCst) {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "[cratonvm] GDB JIT interface disabled: only available on Linux"
            );
        }
        false
    }
}

/// Register `[start, start+len)` as `name`. No-op unless enabled.
pub fn register(start: usize, len: usize, name: &str) {
    if !enabled() {
        return;
    }
    #[cfg(target_os = "linux")]
    linux::register(start, len, name);
}

/// Withdraw the entry for the region starting at `start`, if any.
pub fn unregister(start: usize) {
    if !EVER_REGISTERED.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(target_os = "linux")]
    linux::unregister(start);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Mutex, PoisonError};

    /// `struct jit_code_entry`.
    #[repr(C)]
    pub struct JitCodeEntry {
        next_entry: *mut JitCodeEntry,
        prev_entry: *mut JitCodeEntry,
        symfile_addr: *const u8,
        symfile_size: u64,
    }

    /// `struct jit_descriptor`.
    #[repr(C)]
    pub struct JitDescriptor {
        version: u32,
        action_flag: u32,
        relevant_entry: *mut JitCodeEntry,
        first_entry: *mut JitCodeEntry,
    }

    /// The debugger reads this by name. Mutated only under [`REGISTRY`]'s lock.
    #[no_mangle]
    #[allow(non_upper_case_globals)]
    pub static mut __jit_debug_descriptor: JitDescriptor = JitDescriptor {
        version: 1,
        action_flag: JIT_NOACTION,
        relevant_entry: std::ptr::null_mut(),
        first_entry: std::ptr::null_mut(),
    };

    /// The debugger breakpoints this by name. It must not be inlined or folded
    /// into an identical empty function, hence the volatile read.
    #[no_mangle]
    #[inline(never)]
    pub extern "C" fn __jit_debug_register_code() {
        // SAFETY: a plain read of a `u32` field of a static that lives for the
        // whole process; the caller holds the registry lock.
        let flag = unsafe {
            std::ptr::read_volatile(std::ptr::addr_of!(__jit_debug_descriptor.action_flag))
        };
        std::hint::black_box(flag);
    }

    /// One registered object file and the list node pointing at it.
    struct Entry {
        node: JitCodeEntry,
        _elf: Vec<u8>,
    }

    /// Entries by code start. The pointers are `Box::into_raw` results owned
    /// by this map; they are only touched with the lock held.
    struct Registry {
        by_start: HashMap<usize, *mut Entry>,
    }

    // SAFETY: the raw pointers are uniquely owned by the map and only
    // dereferenced under the mutex that wraps it.
    unsafe impl Send for Registry {}

    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

    fn registry() -> &'static Mutex<Registry> {
        REGISTRY.get_or_init(|| {
            Mutex::new(Registry {
                by_start: HashMap::new(),
            })
        })
    }

    pub(super) fn register(start: usize, len: usize, name: &str) {
        let elf = build_elf(start as u64, len as u64, name);
        let entry = Box::into_raw(Box::new(Entry {
            node: JitCodeEntry {
                next_entry: std::ptr::null_mut(),
                prev_entry: std::ptr::null_mut(),
                symfile_addr: elf.as_ptr(),
                symfile_size: elf.len() as u64,
            },
            // Moving the Vec moves only its header; `symfile_addr` points at
            // its heap buffer, which does not move.
            _elf: elf,
        }));
        let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
        // An address registered twice without a free in between would leave
        // the debugger with two symbols for one range; drop the older one.
        if let Some(old) = reg.by_start.remove(&start) {
            // SAFETY: `old` came from `Box::into_raw` below and was linked.
            unsafe { unlink_and_free(old) };
        }
        // SAFETY: `entry` is a fresh, uniquely owned allocation; the list and
        // descriptor are only mutated with the registry lock held.
        unsafe {
            let node = std::ptr::addr_of_mut!((*entry).node);
            let d = std::ptr::addr_of_mut!(__jit_debug_descriptor);
            let first = (*d).first_entry;
            (*node).next_entry = first;
            if !first.is_null() {
                (*first).prev_entry = node;
            }
            (*d).first_entry = node;
            (*d).relevant_entry = node;
            (*d).action_flag = JIT_REGISTER_FN;
            __jit_debug_register_code();
            (*d).action_flag = JIT_NOACTION;
        }
        reg.by_start.insert(start, entry);
        EVER_REGISTERED.store(true, Ordering::Relaxed);
    }

    pub(super) fn unregister(start: usize) {
        let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = reg.by_start.remove(&start) {
            // SAFETY: `entry` came from `Box::into_raw` in `register` and is
            // still linked.
            unsafe { unlink_and_free(entry) };
        }
    }

    /// Unlink `entry`, tell the debugger, then free it.
    ///
    /// # Safety
    /// `entry` is a linked `Box::into_raw` result from [`register`], and the
    /// registry lock is held.
    unsafe fn unlink_and_free(entry: *mut Entry) {
        let node = std::ptr::addr_of_mut!((*entry).node);
        let d = std::ptr::addr_of_mut!(__jit_debug_descriptor);
        let prev = (*node).prev_entry;
        let next = (*node).next_entry;
        if prev.is_null() {
            (*d).first_entry = next;
        } else {
            (*prev).next_entry = next;
        }
        if !next.is_null() {
            (*next).prev_entry = prev;
        }
        // The debugger reads the entry while stopped in the call, so it is
        // freed only afterwards.
        (*d).relevant_entry = node;
        (*d).action_flag = JIT_UNREGISTER_FN;
        __jit_debug_register_code();
        (*d).action_flag = JIT_NOACTION;
        (*d).relevant_entry = std::ptr::null_mut();
        drop(Box::from_raw(entry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    struct Section {
        name: u32,
        kind: u32,
        flags: u64,
        addr: u64,
        offset: usize,
        size: usize,
        link: u32,
        info: u32,
        entsize: u64,
    }

    fn sections(elf: &[u8]) -> Vec<Section> {
        let shoff = u64_at(elf, 0x28) as usize;
        let shentsize = u16_at(elf, 0x3A) as usize;
        let shnum = u16_at(elf, 0x3C) as usize;
        (0..shnum)
            .map(|i| {
                let h = shoff + i * shentsize;
                Section {
                    name: u32_at(elf, h),
                    kind: u32_at(elf, h + 4),
                    flags: u64_at(elf, h + 8),
                    addr: u64_at(elf, h + 16),
                    offset: u64_at(elf, h + 24) as usize,
                    size: u64_at(elf, h + 32) as usize,
                    link: u32_at(elf, h + 40),
                    info: u32_at(elf, h + 44),
                    entsize: u64_at(elf, h + 56),
                }
            })
            .collect()
    }

    fn cstr(bytes: &[u8], at: usize) -> &str {
        let end = bytes[at..].iter().position(|&b| b == 0).expect("NUL") + at;
        std::str::from_utf8(&bytes[at..end]).expect("utf-8")
    }

    #[test]
    fn the_object_parses_back_to_the_symbol_it_was_built_for() {
        let start = 0x7f12_3456_7000u64;
        let size = 0x2a0u64;
        let name = "java/lang/String.hashCode()I [c2]";
        let elf = build_elf(start, size, name);

        assert_eq!(&elf[0..4], b"\x7fELF");
        assert_eq!(elf[4], 2, "ELFCLASS64");
        assert_eq!(elf[5], 1, "little-endian");
        assert_eq!(u16_at(&elf, 16), 1, "ET_REL");
        assert_eq!(u16_at(&elf, 0x34) as usize, ELF_HEADER_SIZE);
        assert_eq!(u16_at(&elf, 0x3A) as usize, SECTION_HEADER_SIZE);
        assert_eq!(u16_at(&elf, 0x3C) as usize, SECTION_COUNT);
        assert_eq!(u16_at(&elf, 0x3E), SHSTRTAB_SECTION);

        let secs = sections(&elf);
        assert_eq!(secs.len(), 5);
        let shstr = &secs[SHSTRTAB_SECTION as usize];
        let shstr_bytes = &elf[shstr.offset..shstr.offset + shstr.size];
        let names: Vec<&str> = secs.iter().map(|s| cstr(shstr_bytes, s.name as usize)).collect();
        assert_eq!(names, ["", ".text", ".symtab", ".strtab", ".shstrtab"]);

        let text = &secs[TEXT_SECTION as usize];
        assert_eq!(text.kind, SHT_NOBITS);
        assert_eq!(text.flags, SHF_ALLOC | SHF_EXECINSTR);
        assert_eq!(text.addr, start);
        assert_eq!(text.size as u64, size);

        let symtab = &secs[SYMTAB_SECTION as usize];
        assert_eq!(symtab.kind, SHT_SYMTAB);
        assert_eq!(symtab.entsize as usize, SYMBOL_SIZE);
        assert_eq!(symtab.size, 2 * SYMBOL_SIZE);
        assert_eq!(symtab.info, 1);
        let strtab = &secs[symtab.link as usize];
        assert_eq!(strtab.kind, SHT_STRTAB);
        let strtab_bytes = &elf[strtab.offset..strtab.offset + strtab.size];

        let sym = symtab.offset + SYMBOL_SIZE;
        assert_eq!(cstr(strtab_bytes, u32_at(&elf, sym) as usize), name);
        assert_eq!(elf[sym + 4], (STB_GLOBAL << 4) | STT_FUNC);
        assert_eq!(u16_at(&elf, sym + 6), TEXT_SECTION);
        let value = u64_at(&elf, sym + 8);
        // ET_REL: the address is the section's address plus the value.
        assert_eq!(secs[u16_at(&elf, sym + 6) as usize].addr + value, start);
        assert_eq!(u64_at(&elf, sym + 16), size);

        // Every file-backed section lies inside the image.
        for s in &secs[2..] {
            assert!(s.offset + s.size <= elf.len());
        }
        assert_ne!(text.kind, SHT_PROGBITS);
    }

    #[test]
    fn nul_bytes_in_a_name_are_dropped() {
        let elf = build_elf(0x1000, 0x10, "a\0b");
        let secs = sections(&elf);
        let strtab = &secs[STRTAB_SECTION as usize];
        assert_eq!(&elf[strtab.offset..strtab.offset + strtab.size], b"\0ab\0");
    }
}
