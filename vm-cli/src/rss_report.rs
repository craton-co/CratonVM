// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s6/rs: the resident set BY MAPPING at exit, printed by the VM itself
//! (`CRATONVM_DBG_RSS_BY_MAPPING=1`).
//!
//! # Why
//!
//! `tools/rss-by-mapping.sh` (zr z7/rs) needs to start the VM itself and
//! snapshot at a marker, so it cannot sit inside the orchestrator's
//! `/usr/bin/time` A/B rows. This report is taken by the process at the start
//! of its shutdown reports (`maybe_dump_shutdown_reports`, both exit arms) and
//! printed on stderr, so every row of an interleaved A/B can carry its own
//! split next to its `%M` and CPU. Linux only (`/proc/self/smaps`); elsewhere
//! it prints `unsupported=1`.
//!
//! # Lines (stderr)
//!
//! ```text
//! [cratonvm] rss-rollup: vmrss_kb= vmhwm_kb= rss_anon_kb= rss_file_kb= threads= anon_huge_kb= maps=
//! [cratonvm] rss-class: class=<c> maps=N rss_kb= anon_huge_kb= priv_dirty_kb=
//! [cratonvm] rss-top: rss_kb= size_kb= anon_huge_kb= perms=.. name=..
//! ```
//!
//! `vmhwm_kb` is the kernel's peak, i.e. what `/usr/bin/time %M` reports; the
//! classes are the resident set NOW (at exit), which for the boot rows
//! (`Zr7RsBoot hello`, `classes N`) holds everything the boot made. The
//! classes are those of the script, so the two can be read side by side:
//! `exe:text|rodata|data`, `alloc:mimalloc` (`[anon:mimalloc]`, the arenas the
//! launcher names), `anon:<name>` (other named anonymous mappings),
//! `anon:brk`, `stack:main`, `anon:thread-stack` (rw right above a guard of at
//! most 1 MiB), `anon:exec(jit-code)`, `anon:reserved`, `anon:other`,
//! `jdk:jimage`, `jdk:jmod`, `files:jar`, `libs:other`, `files:other`,
//! `kernel`.
//!
//! Reading `/proc` allocates a few hundred KiB, after `vmhwm_kb` was read
//! (the first line), so it cannot move the peak it reports.

/// Sums for one mapping class, KiB.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ClassSum {
    pub maps: usize,
    pub rss_kb: u64,
    pub anon_huge_kb: u64,
    pub priv_dirty_kb: u64,
}

/// One mapping of `/proc/<pid>/smaps`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub lo: u64,
    pub hi: u64,
    pub perms: String,
    pub name: String,
    pub rss_kb: u64,
    pub anon_huge_kb: u64,
    pub priv_dirty_kb: u64,
}

/// Parse `/proc/<pid>/smaps` text into mappings. Header lines are
/// `lo-hi perms offset dev inode [name]`; the per-field lines that follow are
/// `Key:   N kB`. Unknown lines are skipped, so a kernel that adds fields
/// (or a `VmFlags:` line) is read the same.
pub(crate) fn parse_smaps(text: &str) -> Vec<Mapping> {
    let mut out: Vec<Mapping> = Vec::new();
    for line in text.lines() {
        if let Some(m) = parse_header(line) {
            out.push(m);
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        let mut it = line.split_whitespace();
        let (Some(key), Some(val)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(kb) = val.parse::<u64>() else { continue };
        match key {
            "Rss:" => cur.rss_kb = kb,
            "AnonHugePages:" => cur.anon_huge_kb = kb,
            "Private_Dirty:" => cur.priv_dirty_kb = kb,
            _ => {}
        }
    }
    out
}

fn parse_header(line: &str) -> Option<Mapping> {
    let mut it = line.split_whitespace();
    let range = it.next()?;
    let (lo, hi) = range.split_once('-')?;
    let lo = u64::from_str_radix(lo, 16).ok()?;
    let hi = u64::from_str_radix(hi, 16).ok()?;
    let perms = it.next()?;
    if perms.len() != 4 {
        return None;
    }
    let _offset = it.next()?;
    let _dev = it.next()?;
    let _inode = it.next()?;
    // The name may contain blanks (" (deleted)", a path with spaces): it is
    // everything after the fifth field.
    let name = it.collect::<Vec<_>>().join(" ");
    Some(Mapping {
        lo,
        hi,
        perms: perms.to_string(),
        name,
        ..Default::default()
    })
}

/// The class of mapping `i` (the previous mapping decides a thread stack).
pub(crate) fn classify(maps: &[Mapping], i: usize, exe: &str) -> String {
    let m = &maps[i];
    let name = m.name.as_str();
    let exec = m.perms.as_bytes()[2] == b'x';
    let write = m.perms.as_bytes()[1] == b'w';
    match name {
        "[heap]" => return "anon:brk".into(),
        "[stack]" => return "stack:main".into(),
        "[vdso]" | "[vvar]" | "[vsyscall]" | "[vvar_vclock]" => return "kernel".into(),
        _ => {}
    }
    if let Some(inner) = name
        .strip_prefix("[anon:")
        .and_then(|s| s.strip_suffix(']'))
    {
        if inner == "mimalloc" {
            return "alloc:mimalloc".into();
        }
        return format!("anon:{inner}");
    }
    if !name.is_empty() {
        let path = name.strip_suffix(" (deleted)").unwrap_or(name);
        if !exe.is_empty() && path == exe {
            return if exec {
                "exe:text".into()
            } else if write {
                "exe:data".into()
            } else {
                "exe:rodata".into()
            };
        }
        if path.ends_with("/lib/modules") {
            return "jdk:jimage".into();
        }
        if path.ends_with(".jmod") {
            return "jdk:jmod".into();
        }
        if path.ends_with(".jar") {
            return "files:jar".into();
        }
        if path.ends_with(".so") || path.contains(".so.") {
            return "libs:other".into();
        }
        if path.starts_with("/memfd:") || path.starts_with("/dev/shm/") {
            return "shmem:other".into();
        }
        return "files:other".into();
    }
    if exec {
        return "anon:exec(jit-code)".into();
    }
    if m.perms.starts_with("---") {
        return "anon:reserved".into();
    }
    if write && i > 0 {
        let p = &maps[i - 1];
        if p.name.is_empty() && p.perms.starts_with("---") && p.hi == m.lo && p.hi - p.lo <= 1 << 20
        {
            return "anon:thread-stack".into();
        }
    }
    "anon:other".into()
}

/// Sum the mappings per class, largest resident first.
pub(crate) fn by_class(maps: &[Mapping], exe: &str) -> Vec<(String, ClassSum)> {
    let mut sums: Vec<(String, ClassSum)> = Vec::new();
    for i in 0..maps.len() {
        let c = classify(maps, i, exe);
        let m = &maps[i];
        let slot = match sums.iter().position(|(k, _)| *k == c) {
            Some(j) => j,
            None => {
                sums.push((c, ClassSum::default()));
                sums.len() - 1
            }
        };
        let s = &mut sums[slot].1;
        s.maps += 1;
        s.rss_kb += m.rss_kb;
        s.anon_huge_kb += m.anon_huge_kb;
        s.priv_dirty_kb += m.priv_dirty_kb;
    }
    sums.sort_by(|a, b| b.1.rss_kb.cmp(&a.1.rss_kb).then(a.0.cmp(&b.0)));
    sums
}

/// `Key:  N kB` from `/proc/self/status`; -1 when absent.
pub(crate) fn status_kb(status: &str, key: &str) -> i64 {
    status
        .lines()
        .find_map(|l| {
            let rest = l.strip_prefix(key)?.strip_prefix(':')?;
            rest.split_whitespace().next()?.parse::<i64>().ok()
        })
        .unwrap_or(-1)
}

/// Render the report's lines from the three `/proc` texts.
pub(crate) fn render(status: &str, smaps: &str, exe: &str, top: usize) -> Vec<String> {
    let maps = parse_smaps(smaps);
    let huge: u64 = maps.iter().map(|m| m.anon_huge_kb).sum();
    let mut lines = vec![format!(
        "[cratonvm] rss-rollup: vmrss_kb={} vmhwm_kb={} rss_anon_kb={} rss_file_kb={} \
         threads={} anon_huge_kb={} maps={}",
        status_kb(status, "VmRSS"),
        status_kb(status, "VmHWM"),
        status_kb(status, "RssAnon"),
        status_kb(status, "RssFile"),
        status
            .lines()
            .find_map(|l| l.strip_prefix("Threads:"))
            .map(str::trim)
            .unwrap_or("-1"),
        huge,
        maps.len()
    )];
    for (c, s) in by_class(&maps, exe) {
        if s.rss_kb == 0 {
            continue;
        }
        lines.push(format!(
            "[cratonvm] rss-class: class={c} maps={} rss_kb={} anon_huge_kb={} priv_dirty_kb={}",
            s.maps, s.rss_kb, s.anon_huge_kb, s.priv_dirty_kb
        ));
    }
    let mut order: Vec<usize> = (0..maps.len()).collect();
    order.sort_by(|&a, &b| maps[b].rss_kb.cmp(&maps[a].rss_kb).then(a.cmp(&b)));
    for &i in order.iter().take(top) {
        let m = &maps[i];
        if m.rss_kb == 0 {
            break;
        }
        lines.push(format!(
            "[cratonvm] rss-top: rss_kb={} size_kb={} anon_huge_kb={} perms={} class={} name={}",
            m.rss_kb,
            (m.hi - m.lo) / 1024,
            m.anon_huge_kb,
            m.perms,
            classify(&maps, i, exe),
            if m.name.is_empty() { "-" } else { &m.name }
        ));
    }
    lines
}

/// Is the report asked for? `CRATONVM_DBG_RSS_BY_MAPPING` set and not an off
/// word (`0`, `off`, `false`, `no`).
pub(crate) fn requested() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_RSS_BY_MAPPING") {
        None => false,
        Some(v) => {
            let v = v.to_string_lossy();
            let v = v.trim();
            !(v.is_empty()
                || v == "0"
                || v.eq_ignore_ascii_case("off")
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no"))
        }
    }
}

/// Print the report on stderr when asked for (once per process).
pub(crate) fn print_exit_report(owning_vm: Option<&cratonvm_vm::SharedVm>) {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !requested() || DONE.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    // zt t3/fp: the platform-independent footprint counters first, so a
    // Windows run (no smaps: `unsupported=1` below) prints them too.
    // zs s9/rs2: the C2 standby workers (`CRATONVM_TIER_C2_STANDBY`), of the
    // owning VM's compiler (JIT r23 wave 11; they were process counters).
    eprintln!(
        "{}",
        cratonvm_jit::tiered::c2_standby_line(owning_vm.map(|vm| &vm.jit.tiered_manager))
    );
    // zt t2/fp: the deferred vtable dispatch snapshots
    // (`CRATONVM_VTABLE_LAZY_DISPATCH`), of the owning VM's manager (zt t6/rv3).
    // zt t31/x10 (page `zgc-zt13x10-review-residuals-*` item 4, the zt7x4
    // page's item 8): a bounded read -- a thread still holding the write lock
    // at exit must not hang the report; a busy lock prints `on=? ..`.
    {
        let manager = owning_vm.map(|vm| {
            vm.classes
                .vtable_manager
                .try_read_for(std::time::Duration::from_millis(100))
        });
        match manager {
            Some(None) => {
                eprintln!("[vm] vtable-lazy-dispatch: on=? deferred=? built=? (lock busy)")
            }
            manager => eprintln!(
                "{}",
                cratonvm_vm::runtime::vtable::lazy_dispatch_line(manager.flatten().as_deref())
            ),
        }
    }
    // zt t3/fp: the padded-bytecode memo's limit and evictions
    // (`CRATONVM_PADDED_CODE_MEMO_KEEP_LIVE`).
    eprintln!("{}", cratonvm_vm::runtime::frame::padded_code_memo_line());
    if !cfg!(target_os = "linux") {
        eprintln!("[cratonvm] rss-rollup: unsupported=1");
        return;
    }
    // Status first: its VmHWM must not include what reading smaps allocates.
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
    let exe = std::fs::read_link("/proc/self/exe")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    for line in render(&status, &smaps, &exe, 10) {
        eprintln!("{line}");
    }
    // zs s6/rs: the allocator's own census, when it was kept.
    if let Some(line) = crate::sparse_block_alloc::census_line() {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMAPS: &str = "\
55d0a0000000-55d0a2000000 r--p 00000000 08:01 123 /opt/cvm/cratonvm
Size:              32768 kB
Rss:                8000 kB
Private_Dirty:         0 kB
AnonHugePages:         0 kB
55d0a2000000-55d0a6000000 r-xp 02000000 08:01 123 /opt/cvm/cratonvm
Rss:               30000 kB
55d0a6000000-55d0a6400000 rw-p 06000000 08:01 123 /opt/cvm/cratonvm
Rss:                1000 kB
Private_Dirty:      1000 kB
20000000000-20040000000 rw-p 00000000 00:00 0 [anon:mimalloc]
Rss:              141312 kB
Private_Dirty:    141312 kB
AnonHugePages:    141312 kB
VmFlags: rd wr mr mw me ac hg
7f0000000000-7f0000001000 ---p 00000000 00:00 0
Rss:                   0 kB
7f0000001000-7f0000201000 rw-p 00000000 00:00 0
Rss:                  40 kB
7f1000000000-7f1000010000 rwxp 00000000 00:00 0
Rss:                  64 kB
7f2000000000-7f2009000000 r--s 00000000 08:01 77 /usr/lib/jvm/jdk-25/lib/modules
Rss:                9900 kB
7f3000000000-7f3000100000 rw-p 00000000 00:00 0
Rss:                 300 kB
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0 [stack]
Rss:                 100 kB
";

    #[test]
    fn zs6rs_smaps_classes_follow_the_script() {
        let maps = parse_smaps(SMAPS);
        assert_eq!(maps.len(), 10);
        assert_eq!(maps[3].rss_kb, 141_312);
        assert_eq!(maps[3].anon_huge_kb, 141_312);
        assert_eq!(maps[3].lo, 0x200_0000_0000);
        let exe = "/opt/cvm/cratonvm";
        let classes: Vec<String> = (0..maps.len()).map(|i| classify(&maps, i, exe)).collect();
        let want = [
            "exe:rodata",
            "exe:text",
            "exe:data",
            "alloc:mimalloc",
            "anon:reserved",
            "anon:thread-stack",
            "anon:exec(jit-code)",
            "jdk:jimage",
            "anon:other",
            "stack:main",
        ];
        assert_eq!(classes, want);
        // Without the exe path the binary's pages are plain files.
        assert_eq!(classify(&maps, 1, ""), "files:other");
    }

    #[test]
    fn zs6rs_rollup_and_classes_sum_rss() {
        let status = "Name:\tcratonvm\nThreads:\t13\nVmHWM:\t  218848 kB\nVmRSS:\t  200000 kB\nRssAnon:\t  150000 kB\nRssFile:\t   50000 kB\n";
        let lines = render(status, SMAPS, "/opt/cvm/cratonvm", 3);
        assert!(lines[0].starts_with("[cratonvm] rss-rollup: vmrss_kb=200000 vmhwm_kb=218848"));
        assert!(lines[0].contains("threads=13"));
        assert!(lines[0].contains("anon_huge_kb=141312"));
        assert_eq!(
            lines[1],
            "[cratonvm] rss-class: class=alloc:mimalloc maps=1 rss_kb=141312 \
             anon_huge_kb=141312 priv_dirty_kb=141312"
        );
        assert!(lines
            .iter()
            .any(|l| l.contains("class=exe:text maps=1 rss_kb=30000")));
        assert!(lines
            .iter()
            .any(|l| l.contains("class=jdk:jimage maps=1 rss_kb=9900")));
        let tops: Vec<_> = lines.iter().filter(|l| l.contains("rss-top")).collect();
        assert_eq!(tops.len(), 3);
        assert!(tops[0].contains("rss_kb=141312 size_kb=1048576"));
        assert!(tops[0].contains("name=[anon:mimalloc]"));
    }

    #[test]
    fn zs6rs_status_keys_and_missing_keys() {
        assert_eq!(status_kb("VmHWM:\t  12 kB\n", "VmHWM"), 12);
        assert_eq!(status_kb("VmHWM:\t  12 kB\n", "VmRSS"), -1);
        // A key that is a prefix of another is not confused with it.
        assert_eq!(status_kb("VmRSSx:\t 5 kB\nVmRSS:\t 7 kB\n", "VmRSS"), 7);
    }
}
