// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Linux x86-64 home of the four per-thread JIT mirrors (frame-record RBP,
//! compile id, JIT thread, loop-poll word) that generated code reaches as
//! `fs:[disp32]`.
//!
//! A Rust `thread_local!` is at a fixed `fs:` offset only in static TLS (the
//! launcher). In a `dlopen`ed `libcratonvm.so` it is dynamic TLS, the proof
//! refuses it, and every compiled call pays the helper-call frame record
//! (t1 JDISP J-3). Linking the whole library for initial-exec instead makes
//! `dlopen` fail (its 155 KB TLS segment exceeds glibc's static-TLS surplus),
//! so when the proof fails we load a 64-byte TLS-only shared object built in
//! memory ([`shim_elf`]): `DF_STATIC_TLS` costs only that block, glibc gives it
//! one fixed offset on every thread, and the same two-thread proof vouches
//! for it. Any failure leaves the displacement 0 (the helper-call path).

use std::cell::Cell;
use std::sync::OnceLock;

const SLOTS: usize = 4;

thread_local! {
    static BLOCK: [Cell<usize>; SLOTS] = const { [const { Cell::new(0) }; SLOTS] };
}

/// How generated code reaches the block: `delta` is the `fs:` offset of slot
/// 0, `shim` says the block is the shim's, not `BLOCK`.
#[derive(Clone, Copy)]
struct Scheme {
    delta: i32,
    shim: bool,
}

// The proof is per process (the library's TLS model is), like the DISP caches.
static SCHEME: OnceLock<Option<Scheme>> = OnceLock::new();

/// One mirror: slot index into the block.
#[derive(Clone, Copy)]
pub(super) struct Mirror(pub(super) usize);

impl Mirror {
    pub(super) fn with<R>(self, f: impl FnOnce(&Cell<usize>) -> R) -> R {
        match SCHEME.get() {
            Some(Some(Scheme { delta, shim: true })) => f(shim_cell(*delta, self.0)),
            _ => BLOCK.with(|b| f(&b[self.0])),
        }
    }

    pub(super) fn try_with<R>(self, f: impl FnOnce(&Cell<usize>) -> R) -> Result<R, ()> {
        match SCHEME.get() {
            Some(Some(Scheme { delta, shim: true })) => Ok(f(shim_cell(*delta, self.0))),
            _ => BLOCK.try_with(|b| f(&b[self.0])).map_err(|_| ()),
        }
    }

    /// The `fs:` displacement of this mirror as the raw disp32 bits, or 0.
    pub(super) fn disp(self) -> usize {
        match SCHEME.get_or_init(resolve) {
            Some(s) => (s.delta + 8 * self.0 as i32) as u32 as usize,
            None => 0,
        }
    }
}

fn shim_cell(delta: i32, slot: usize) -> &'static Cell<usize> {
    let base = cratonvm_types::fs_tls_proof::fs_base() as isize + delta as isize;
    // SAFETY: `delta` was proved on two threads to name this thread's shim
    // block (8-aligned, `SLOTS` words, alive as long as the thread).
    unsafe { &*((base as usize + 8 * slot) as *const Cell<usize>) }
}

const SENTINEL: usize = 0x4D49_5252_4F52_5348;

fn resolve() -> Option<Scheme> {
    let own = cratonvm_types::fs_tls_proof::prove_fs_disp(
        || BLOCK.try_with(|b| b.as_ptr() as usize).ok(),
        SENTINEL,
    );
    if let Some(delta) = own {
        return Some(Scheme { delta, shim: false });
    }
    load_shim().map(|delta| Scheme { delta, shim: true })
}

/// Load the TLS shim and prove its block; the `fs:` offset of slot 0.
pub(super) fn load_shim() -> Option<i32> {
    extern "C" {
        fn memfd_create(name: *const core::ffi::c_char, flags: u32) -> i32;
        fn write(fd: i32, buf: *const core::ffi::c_void, n: usize) -> isize;
        fn close(fd: i32) -> i32;
        fn dlopen(path: *const core::ffi::c_char, flags: i32) -> *mut core::ffi::c_void;
        fn dlinfo(h: *mut core::ffi::c_void, req: i32, out: *mut core::ffi::c_void) -> i32;
        fn dlerror() -> *const core::ffi::c_char;
        fn __tls_get_addr(ti: *const usize) -> *mut u8;
    }
    const RTLD_NOW: i32 = 2;
    const RTLD_DI_TLS_MODID: i32 = 9;
    let elf = shim_elf();
    // SAFETY: plain libc calls on a buffer and an fd we own; the handle is
    // leaked on purpose (the block must outlive every thread).
    let handle = unsafe {
        let fd = memfd_create(c"cvm-tls".as_ptr(), 0);
        if fd < 0 {
            return None;
        }
        let ok = write(fd, elf.as_ptr().cast(), elf.len()) == elf.len() as isize;
        let path = format!("/proc/self/fd/{fd}\0");
        let h = if ok {
            dlopen(path.as_ptr().cast(), RTLD_NOW)
        } else {
            core::ptr::null_mut()
        };
        close(fd);
        h
    };
    if handle.is_null() {
        if false {
            // SAFETY: dlerror returns a NUL-terminated message or null.
            let e = unsafe { dlerror() };
            if !e.is_null() {
                // SAFETY: dlerror's non-null message is NUL-terminated and still live.
                eprintln!("[INLINE-FR] TLS shim dlopen failed: {:?}", unsafe {
                    core::ffi::CStr::from_ptr(e)
                });
            }
        }
        return None;
    }
    let mut modid = 0usize;
    // SAFETY: RTLD_DI_TLS_MODID writes one size_t.
    if unsafe { dlinfo(handle, RTLD_DI_TLS_MODID, (&mut modid as *mut usize).cast()) } != 0
        || modid == 0
    {
        return None;
    }
    // `__tls_get_addr` (not `dlinfo(TLS_DATA)`, which is null until the thread's dtv catches up) is
    // the per-thread address of the module block, static or not; the two-thread proof tells which.
    let block = move || {
        let ti = [modid, 0usize];
        // SAFETY: `ti` is a valid tls_index for a loaded TLS module.
        let p = unsafe { __tls_get_addr(ti.as_ptr()) };
        (!p.is_null()).then_some(p as usize)
    };
    cratonvm_types::fs_tls_proof::prove_fs_disp_with(&block, SENTINEL ^ 1)
}

/// An ET_DYN with one PT_TLS (64 zeroed bytes), `DF_STATIC_TLS`, no code and
/// no relocations: just enough for glibc to map it and place its TLS block
/// in static TLS.
pub(super) fn shim_elf() -> Vec<u8> {
    const PH: u64 = 64;
    const SYM: u64 = PH + 4 * 56; // null Elf64_Sym + one local TLS symbol
    const STR: u64 = SYM + 48; // one NUL
    const HASH: u64 = STR + 8; // nbucket=1 nchain=1 bucket=0 chain=0
    const DYN: u64 = HASH + 24;
    const DYN_END: u64 = DYN + 10 * 16;
    const RELA: u64 = DYN_END; // one R_X86_64_TPOFF64 against the TLS symbol...
    const WORD: u64 = RELA + 24; // ...into this word: its only job is to make glibc place the block in STATIC TLS
    const END: u64 = WORD + 8;
    const TLS: u64 = END.next_multiple_of(64);
    let mut b = Vec::new();
    let u16s = |b: &mut Vec<u8>, v: u16| b.extend_from_slice(&v.to_le_bytes());
    let u32s = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_le_bytes());
    let u64s = |b: &mut Vec<u8>, v: u64| b.extend_from_slice(&v.to_le_bytes());
    b.extend_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    u16s(&mut b, 3); // ET_DYN
    u16s(&mut b, 62); // x86-64
    u32s(&mut b, 1);
    u64s(&mut b, 0); // entry
    u64s(&mut b, PH);
    u64s(&mut b, 0); // no sections
    u32s(&mut b, 0);
    u16s(&mut b, 64);
    u16s(&mut b, 56);
    u16s(&mut b, 4);
    u16s(&mut b, 64);
    u16s(&mut b, 0);
    u16s(&mut b, 0);
    // (type, flags, offset == vaddr == paddr, filesz, memsz, align)
    let ph = |b: &mut Vec<u8>, t: u32, fl: u32, off: u64, fsz: u64, msz: u64, al: u64| {
        u32s(b, t);
        u32s(b, fl);
        u64s(b, off);
        u64s(b, off);
        u64s(b, off);
        u64s(b, fsz);
        u64s(b, msz);
        u64s(b, al);
    };
    ph(&mut b, 1, 6, 0, TLS, TLS, 0x1000); // PT_LOAD rw: glibc relocates .dynamic in place
    ph(&mut b, 2, 6, DYN, DYN_END - DYN, DYN_END - DYN, 8); // PT_DYNAMIC
    ph(&mut b, 7, 4, TLS, 0, 64, 64); // PT_TLS
    ph(&mut b, 0x6474_E551, 6, 0, 0, 0, 16); // PT_GNU_STACK: no exec stack
    b.resize(SYM as usize + 24, 0); // the null symbol
    u32s(&mut b, 0); // st_name
    b.extend_from_slice(&[6, 0]); // STB_LOCAL | STT_TLS, default visibility
    u16s(&mut b, 1); // st_shndx: any defined section
    u64s(&mut b, 0);
    u64s(&mut b, 8);
    b.resize(HASH as usize, 0);
    for v in [1u32, 2, 0, 0, 0] {
        u32s(&mut b, v); // DT_HASH table
    }
    b.resize(DYN as usize, 0);
    for (tag, val) in [
        (4u64, HASH),
        (5, STR),
        (6, SYM),
        (10, 1),
        (11, 24),
        (30, 0x10),
        (7, RELA),
        (8, 24),
        (9, 24),
        (0, 0),
    ] {
        u64s(&mut b, tag);
        u64s(&mut b, val);
    }
    u64s(&mut b, WORD);
    u64s(&mut b, (1 << 32) | 18); // R_X86_64_TPOFF64 against symbol 1
    u64s(&mut b, 0);
    b.resize(TLS as usize, 0);
    b
}

#[cfg(test)]
mod tests;
