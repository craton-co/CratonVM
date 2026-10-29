// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Test-only in-process stack sampler (Windows x64), for profiling a bench on
//! a Windows host without `perf` or an elevated ETW session: a helper thread
//! suspends the measured thread every ~0.5 ms, unwinds it with
//! `RtlVirtualUnwind`, and symbolizes the frames with dbghelp against the
//! test binary's PDB. Self and inclusive histograms by function name. Frames
//! inlined by the optimizer are attributed to their caller, and the linker's
//! identical-code folding can name a folded function after a sibling.
#![allow(unsafe_code, clippy::all)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::*;
use windows_sys::Win32::System::Threading::*;

#[repr(C, align(16))]
struct AlignedCtx(CONTEXT);

pub struct Profile {
    pub stacks: Vec<Vec<u64>>,
}

pub fn profile<R>(work: impl FnOnce() -> R) -> (R, Profile) {
    let tid = unsafe { GetCurrentThreadId() };
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let sampler = std::thread::spawn(move || {
        let h: HANDLE = unsafe { OpenThread(THREAD_ALL_ACCESS, 0, tid) };
        let mut stacks = Vec::new();
        while !stop2.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_micros(500));
            unsafe {
                if SuspendThread(h) == u32::MAX {
                    continue;
                }
                let mut ctx: AlignedCtx = std::mem::zeroed();
                ctx.0.ContextFlags = CONTEXT_FULL_AMD64;
                let mut frames = Vec::with_capacity(64);
                if GetThreadContext(h, &mut ctx.0) != 0 {
                    for _ in 0..96 {
                        let rip = ctx.0.Rip;
                        if rip == 0 {
                            break;
                        }
                        frames.push(rip);
                        let mut base = 0u64;
                        let entry = RtlLookupFunctionEntry(rip, &mut base, std::ptr::null_mut());
                        if entry.is_null() {
                            // leaf: return address at rsp
                            let rsp = ctx.0.Rsp as *const u64;
                            if rsp.is_null() {
                                break;
                            }
                            ctx.0.Rip = *rsp;
                            ctx.0.Rsp += 8;
                        } else {
                            let mut hd = std::ptr::null_mut();
                            let mut est = 0u64;
                            RtlVirtualUnwind(
                                0,
                                base,
                                rip,
                                entry,
                                &mut ctx.0,
                                &mut hd,
                                &mut est,
                                std::ptr::null_mut(),
                            );
                        }
                    }
                }
                ResumeThread(h);
                if !frames.is_empty() {
                    stacks.push(frames);
                }
            }
        }
        unsafe { CloseHandle(h) };
        stacks
    });
    let r = work();
    stop.store(true, Ordering::Relaxed);
    let stacks = sampler.join().unwrap();
    (r, Profile { stacks })
}

impl Profile {
    pub fn report(&self, top: usize) {
        let proc_ = unsafe { GetCurrentProcess() };
        unsafe {
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS);
            let dir = std::env::current_exe().unwrap();
            let dir = dir.parent().unwrap().to_string_lossy().into_owned() + "\0";
            SymInitialize(proc_, dir.as_ptr(), 1);
        }
        let mut cache: HashMap<u64, String> = HashMap::new();
        let mut name_of = |addr: u64| -> String {
            if let Some(n) = cache.get(&addr) {
                return n.clone();
            }
            #[repr(C)]
            struct Buf {
                info: SYMBOL_INFO,
                extra: [u8; 1024],
            }
            let mut b: Buf = unsafe { std::mem::zeroed() };
            b.info.SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
            b.info.MaxNameLen = 1024;
            let mut disp = 0u64;
            let n = if unsafe { SymFromAddr(proc_, addr, &mut disp, &mut b.info) } != 0 {
                let len = b.info.NameLen as usize;
                let p = b.info.Name.as_ptr() as *const u8;
                let s = unsafe { std::slice::from_raw_parts(p, len) };
                String::from_utf8_lossy(s).into_owned()
            } else {
                format!("0x{addr:x}")
            };
            cache.insert(addr, n.clone());
            n
        };
        let total = self.stacks.len().max(1);
        let mut self_c: HashMap<String, usize> = HashMap::new();
        let mut incl_c: HashMap<String, usize> = HashMap::new();
        for s in &self.stacks {
            let names: Vec<String> = s.iter().map(|&a| name_of(a)).collect();
            *self_c.entry(names[0].clone()).or_default() += 1;
            let mut seen = std::collections::HashSet::new();
            for n in names {
                if seen.insert(n.clone()) {
                    *incl_c.entry(n).or_default() += 1;
                }
            }
        }
        let print = |title: &str, m: &HashMap<String, usize>| {
            let mut v: Vec<_> = m.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            eprintln!("---- {title} ({total} samples)");
            for (n, c) in v.into_iter().take(top) {
                eprintln!("{:6.2}% {}", *c as f64 * 100.0 / total as f64, n);
            }
        };
        if let Ok(pat) = std::env::var("CVM_PROF_CALLERS") {
            for p in pat.split(',') {
                let mut chains: HashMap<String, usize> = HashMap::new();
                for s in &self.stacks {
                    let names: Vec<String> = s.iter().take(6).map(|&a| name_of(a)).collect();
                    if let Some(pos) = names.iter().position(|n| n.contains(p)) {
                        let chain = names[pos..]
                            .iter()
                            .map(|n| n.chars().take(70).collect::<String>())
                            .collect::<Vec<_>>()
                            .join(" <- ");
                        *chains.entry(chain).or_default() += 1;
                    }
                }
                let mut v: Vec<_> = chains.into_iter().collect();
                v.sort_by(|a, b| b.1.cmp(&a.1));
                eprintln!("---- callers of {p}");
                for (c, n) in v.into_iter().take(8) {
                    eprintln!("{n:5} {c}");
                }
            }
        }
        print("SELF", &self_c);
        print("INCLUSIVE", &incl_c);
    }
}
