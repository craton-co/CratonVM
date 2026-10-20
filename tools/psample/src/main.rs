// psample <pid> <interval_ms> <duration_s> [exe_dir_for_pdb]
// Stack-less RIP sampler: suspend each thread of <pid>, read RIP, resume. Symbolises through dbghelp (PDB).
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Diagnostics::Debug::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Threading::*;

#[repr(C, align(16))]
struct Aligned(CONTEXT);

// Name one address: PDB symbol, else the JIT perf map, else "<anon>".
unsafe fn symname(hp: HANDLE, jitmap: &[(u64, u64, String)], addr: u64) -> String {
    let mut buf = vec![0u8; size_of::<SYMBOL_INFOW>() + 1024 * 2];
    let si = buf.as_mut_ptr() as *mut SYMBOL_INFOW;
    (*si).SizeOfStruct = size_of::<SYMBOL_INFOW>() as u32;
    (*si).MaxNameLen = 1024;
    let mut disp = 0u64;
    if SymFromAddrW(hp, addr, &mut disp, si) != 0 {
        let nm = std::slice::from_raw_parts((*si).Name.as_ptr(), (*si).NameLen as usize);
        return String::from_utf16_lossy(nm);
    }
    match jitmap.iter().rev().find(|(s, z, _)| addr >= *s && addr < *s + *z) {
        Some((_, _, n)) => format!("JIT:{}", n),
        None => format!("<anon {:x}>", addr),
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let pid: u32 = a[1].parse().unwrap();
    let interval: u64 = a[2].parse().unwrap();
    let dur: u64 = a[3].parse().unwrap();
    unsafe {
        let hp = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if hp.is_null() { eprintln!("OpenProcess failed {}", GetLastError()); return; }
        SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_ANYTHING);
        let path: Vec<u16> = a.get(4).map(|s| s.encode_utf16().chain([0]).collect()).unwrap_or_default();
        let pp = if a.len() > 4 { path.as_ptr() } else { std::ptr::null() };
        if SymInitializeW(hp, pp, 1) == 0 { eprintln!("SymInitialize failed {}", GetLastError()); }
        let start = std::time::Instant::now();
        let mut samples: HashMap<u64, u64> = HashMap::new();
        let mut per_thread: HashMap<u32, u64> = HashMap::new();
        let mut per_thread_rip: HashMap<(u32, u64), u64> = HashMap::new();
        let mut total = 0u64;
        // PSAMPLE_STACK=<depth> also walks each sampled thread's native stack (x64 unwind data through
        // dbghelp; it stops at the first JIT frame). PSAMPLE_FILTER=<substr> keeps the stacks whose top
        // frame's symbol contains it. The report prints the most common chains.
        let stack_depth: usize = std::env::var("PSAMPLE_STACK").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut stacks: HashMap<Vec<u64>, u64> = HashMap::new();
        let mut tids: Vec<u32> = vec![];
        let mut last_refresh = std::time::Instant::now();
        while start.elapsed().as_secs() < dur {
            // The system-wide thread snapshot is the slow part of a pass: refresh the
            // pid's thread list once a second instead of on every sample.
            if tids.is_empty() || last_refresh.elapsed().as_millis() >= 1000 {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
                if snap == INVALID_HANDLE_VALUE { eprintln!("snapshot failed {}", GetLastError()); }
                let mut te: THREADENTRY32 = zeroed();
                te.dwSize = size_of::<THREADENTRY32>() as u32;
                tids = vec![];
                let f1 = Thread32First(snap, &mut te); if f1 == 0 && start.elapsed().as_millis() < 50 { eprintln!("Thread32First failed {}", GetLastError()); }
                if f1 != 0 {
                    loop {
                        if te.th32OwnerProcessID == pid { tids.push(te.th32ThreadID); }
                        if Thread32Next(snap, &mut te) == 0 { break; }
                    }
                }
                CloseHandle(snap);
                last_refresh = std::time::Instant::now();
            }
            for &tid in &tids {
                let ht = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, tid);
                if ht.is_null() { if total == 0 && start.elapsed().as_millis() < 50 { eprintln!("OpenThread({}) failed {}", tid, GetLastError()); } continue; }
                let sr = SuspendThread(ht); if sr == u32::MAX && start.elapsed().as_millis() < 50 { eprintln!("Suspend failed {}", GetLastError()); }
                if sr != u32::MAX {
                    let mut actx: Aligned = zeroed();
                    let ctx = &mut actx.0;
                    ctx.ContextFlags = if stack_depth > 0 { CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64 } else { CONTEXT_CONTROL_AMD64 };
                    let gr = GetThreadContext(ht, ctx); if gr == 0 && start.elapsed().as_millis() < 50 { eprintln!("GetContext failed {}", GetLastError()); }
                    if gr != 0 {
                        *samples.entry(ctx.Rip).or_insert(0) += 1;
                        *per_thread.entry(tid).or_insert(0) += 1;
                        *per_thread_rip.entry((tid, ctx.Rip)).or_insert(0) += 1;
                        total += 1;
                        if stack_depth > 0 {
                            let mut fr: STACKFRAME64 = zeroed();
                            fr.AddrPC.Offset = ctx.Rip; fr.AddrPC.Mode = AddrModeFlat;
                            fr.AddrFrame.Offset = ctx.Rbp; fr.AddrFrame.Mode = AddrModeFlat;
                            fr.AddrStack.Offset = ctx.Rsp; fr.AddrStack.Mode = AddrModeFlat;
                            let mut chain = vec![];
                            while chain.len() < stack_depth {
                                let ok = StackWalk64(0x8664u32, hp, ht, &mut fr, ctx as *mut CONTEXT as *mut _, None, Some(SymFunctionTableAccess64), Some(SymGetModuleBase64), None);
                                if ok == 0 || fr.AddrPC.Offset == 0 { break; }
                                chain.push(fr.AddrPC.Offset);
                            }
                            *stacks.entry(chain).or_insert(0) += 1;
                        }
                    }
                    ResumeThread(ht);
                }
                CloseHandle(ht);
            }
            std::thread::sleep(std::time::Duration::from_millis(interval));
        }
        let mut jitmap: Vec<(u64, u64, String)> = vec![];
        if let Ok(tmp) = std::env::var("TEMP") {
            if let Ok(txt) = std::fs::read_to_string(format!("{}/perf-{}.map", tmp, pid)) {
                for l in txt.lines() {
                    let mut it = l.splitn(3, ' ');
                    if let (Some(s), Some(z), Some(n)) = (it.next(), it.next(), it.next()) {
                        if let (Ok(s), Ok(z)) = (u64::from_str_radix(s.trim_start_matches("0x"), 16), u64::from_str_radix(z.trim_start_matches("0x"), 16)) { jitmap.push((s, z, n.to_string())); }
                    }
                }
            }
        }
        eprintln!("jit map entries: {}", jitmap.len());
        // symbolise
        let mut by_sym: HashMap<String, u64> = HashMap::new();
        let mut names: HashMap<u64, String> = HashMap::new();
        let mut buf = vec![0u8; size_of::<SYMBOL_INFOW>() + 1024 * 2];
        for (rip, n) in &samples {
            let si = buf.as_mut_ptr() as *mut SYMBOL_INFOW;
            (*si).SizeOfStruct = size_of::<SYMBOL_INFOW>() as u32;
            (*si).MaxNameLen = 1024;
            let mut disp = 0u64;
            let name = if SymFromAddrW(hp, *rip, &mut disp, si) != 0 {
                let len = (*si).NameLen as usize;
                let nm = std::slice::from_raw_parts((*si).Name.as_ptr(), len);
                let mut mi: IMAGEHLP_MODULEW64 = zeroed();
                mi.SizeOfStruct = size_of::<IMAGEHLP_MODULEW64>() as u32;
                let m = if SymGetModuleInfoW64(hp, *rip, &mut mi) != 0 {
                    let w = &mi.ModuleName; let l = w.iter().position(|c| *c == 0).unwrap_or(w.len()); String::from_utf16_lossy(&w[..l])
                } else { "?".into() };
                format!("{}!{}", m, String::from_utf16_lossy(nm))
            } else {
                let mut mi: IMAGEHLP_MODULEW64 = zeroed();
                mi.SizeOfStruct = size_of::<IMAGEHLP_MODULEW64>() as u32;
                if SymGetModuleInfoW64(hp, *rip, &mut mi) != 0 {
                    let w = &mi.ModuleName; let l = w.iter().position(|c| *c == 0).unwrap_or(w.len()); format!("{}!<nosym>", String::from_utf16_lossy(&w[..l]))
                } else {
                    match jitmap.iter().rev().find(|(s, z, _)| *rip >= *s && *rip < *s + *z) { Some((_, _, n)) => format!("JIT:{}", n), None => "<anon>".into() }
                }
            };
            names.insert(*rip, name.clone());
            *by_sym.entry(name).or_insert(0) += n;
        }
        let mut v: Vec<_> = by_sym.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        println!("total samples {} over {} threads", total, per_thread.len());
        for (k, n) in v.iter().take(60) {
            println!("{:6} {:5.1}%  {}", n, *n as f64 * 100.0 / total as f64, k);
        }

        // Per-thread view: threads that spent time outside the wait primitives.
        let is_wait = |n: &str| n.contains("WaitFor") || n.contains("WaitOnAddress") || n.contains("NtDelayExecution") || n.contains("ZwWait");
        let mut tids: Vec<_> = per_thread.iter().map(|(t, n)| (*t, *n)).collect();
        tids.sort_by_key(|(t, _)| *t);
        for (tid, tn) in tids {
            let mut agg: HashMap<&str, u64> = HashMap::new();
            let mut busy = 0u64;
            for ((t, rip), n) in &per_thread_rip {
                if *t != tid { continue; }
                let nm = names.get(rip).map(|s| s.as_str()).unwrap_or("?");
                if !is_wait(nm) { busy += *n; }
                *agg.entry(nm).or_insert(0) += *n;
            }
            if busy * 10 < tn { continue; }
            println!("-- thread {} busy {}/{}", tid, busy, tn);
            let mut v: Vec<_> = agg.into_iter().filter(|(k, _)| !is_wait(k)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            for (k, n) in v.iter().take(8) { println!("   {:5}  {}", n, k); }
        }

        if stack_depth > 0 {
            let filt = std::env::var("PSAMPLE_FILTER").unwrap_or_default();
            let mut cache: HashMap<u64, String> = HashMap::new();
            let mut agg: HashMap<Vec<String>, u64> = HashMap::new();
            for (chain, n) in &stacks {
                if chain.is_empty() { continue; }
                let names: Vec<String> = chain.iter().map(|a| cache.entry(*a).or_insert_with(|| symname(hp, &jitmap, *a)).clone()).collect();
                if !filt.is_empty() && !names[0].contains(&filt) { continue; }
                *agg.entry(names).or_insert(0) += *n;
            }
            let mut v: Vec<_> = agg.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            println!("== stacks (filter '{}') ==", filt);
            for (names, n) in v.iter().take(25) {
                println!("{:5}  {}", n, names.join("  <-  "));
            }
        }
    }
}
