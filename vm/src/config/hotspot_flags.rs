// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Resolve the modelled flag table from this VM's configuration and arguments.
use cratonvm_native_api::hotspot_flags::HotspotVmFlag;

/// zt t24/x20: [`resolved_flags`] for a running VM: the rows its configuration
/// gives, with the values a `setVMOption` wrote since
/// (`HeapRealm::managed_flags`, origin `{management}`). What `jcmd VM.flags`
/// and the JMX flag surface read; PrintFlagsFinal runs at boot, before any
/// write, and keeps [`resolved_flags`].
pub fn resolved_flags_for(shared: &crate::vm::SharedVm) -> Vec<HotspotVmFlag> {
    let mut rows = resolved_flags(&shared.config);
    shared.mem.managed_flags.apply(&mut rows);
    // zt t26/x21: on ZGC the `SoftMaxHeapSize` row reads the heap's own soft
    // maximum (its `setVMOption` door, `zgc_set_soft_max_heap_flag`, writes
    // the heap, not `managed_flags`), so `VM.flags` shows a written value as
    // the JMX row already does. Same value rule as that row: the ergonomic
    // default is `MaxHeapSize`, anything else clamped to it.
    if let Some((soft, _, kind)) = shared.mem.heap.zgc_soft_max_flag() {
        if let Some(row) = rows.iter_mut().find(|row| row.name == "SoftMaxHeapSize") {
            let max = u64::try_from(shared.config.max_heap_size).unwrap_or(u64::MAX);
            match kind {
                1 => {
                    row.value = soft.min(max).to_string();
                    row.origin = "{command line}";
                }
                2 => {
                    row.value = soft.min(max).to_string();
                    row.origin = "{management}";
                }
                _ => {}
            }
        }
    }
    rows
}

/// `CRATONVM_HOTSPOT_FLAG_TABLE_ROWS` (zt t26/x21, default ON, `=0` kill;
/// every collector, both modes): the modelled flag table carries
/// `AlwaysPreTouch`, `CrashOnOutOfMemoryError`, `DisableExplicitGC`,
/// `ExitOnOutOfMemoryError`, `ExplicitGCInvokesConcurrent`,
/// `SoftMaxHeapSize` (manageable on every collector) and
/// `StackTraceInThrowable`, so `HotSpotDiagnosticMXBean.getVMOption`,
/// `-XX:+PrintFlagsFinal` and `jcmd VM.flags` answer them. `=0`: the rows are
/// absent (`getVMOption` throws `IllegalArgumentException`, and
/// `SoftMaxHeapSize` is listed on ZGC only), as before.
pub fn hotspot_flag_table_rows_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_HOTSPOT_FLAG_TABLE_ROWS")
}

/// `CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS` (zt t27/lg1, default ON, `=0` kill;
/// every collector, both modes): the modelled flag table carries the
/// manageable flags whose run-time effect this VM has (see
/// [`resolved_flags`]), listed writeable, and `setVMOption` writes them into
/// the VM's `managed_flags`, which their consumers read. `=0`: absent
/// (`getVMOption` throws `IllegalArgumentException`), as before.
pub fn manageable_effect_flags_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS")
}

/// zt t31/x10 (page `zgc-zt13x10-review-residuals-*` item 30h):
/// `CRATONVM_GC_FULL_GC_HEAP_DUMPS` (default ON, `=0` kill). HotSpot 25's
/// full-collection heap dumps on G1 and Serial (`HeapDumpBeforeFullGC`,
/// `HeapDumpAfterFullGC`, `FullGCHeapDumpLimit`) and G1's periodic collection
/// (`G1PeriodicGCInterval`, `G1PeriodicGCSystemLoadThreshold`), with their
/// manageable rows listed on every collector. Read at a full collection's
/// door and when the flag table is built; no static.
pub fn full_gc_heap_dumps_enabled() -> bool {
    manageable_effect_flags_enabled()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_FULL_GC_HEAP_DUMPS")
}

/// zt t31/x10: the command line's `-XX:G1PeriodicGCInterval=<ms>` (`0` when
/// absent or unreadable, HotSpot's default). Pure.
pub fn g1_periodic_gc_interval_arg(config: &super::VmConfig) -> u64 {
    jvm_arg_value(config, "G1PeriodicGCInterval")
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0)
}

/// zt t31/x10: the command line's `-XX:G1PeriodicGCSystemLoadThreshold=<d>`
/// (`0.0` when absent, unreadable or negative, HotSpot's default). Pure.
pub fn g1_periodic_gc_load_threshold_arg(config: &super::VmConfig) -> f64 {
    jvm_arg_value(config, "G1PeriodicGCSystemLoadThreshold")
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0)
}

/// zt t27/lg1: the value the command line gives the boolean HotSpot option
/// `name` (the last `-XX:+<name>` / `-XX:-<name>` of `VmConfig::jvm_args`), or
/// `default`. For options the launcher accepts without a `VmConfig` field of
/// their own (`PrintClassHistogram`, `HeapDumpBeforeFullGC`). Pure.
pub fn jvm_arg_bool(config: &super::VmConfig, name: &str, default: bool) -> bool {
    config
        .jvm_args
        .iter()
        .rev()
        .find_map(|arg| {
            let flag = arg.strip_prefix("-XX:")?;
            if flag.strip_prefix('+') == Some(name) {
                Some(true)
            } else if flag.strip_prefix('-') == Some(name) {
                Some(false)
            } else {
                None
            }
        })
        .unwrap_or(default)
}

/// zt t28/mf: the text the command line gives the HotSpot option `name` (the
/// last `-XX:<name>=<value>` of `VmConfig::jvm_args`), if any. Pure.
pub fn jvm_arg_value<'a>(config: &'a super::VmConfig, name: &str) -> Option<&'a str> {
    config.jvm_args.iter().rev().find_map(|arg| {
        arg.strip_prefix("-XX:")?
            .strip_prefix(name)?
            .strip_prefix('=')
    })
}

/// zt t28/mf: HotSpot's `HeapDumpGzipLevel` (`int`, `range(0, 9)`, default
/// 0) from the command line: the launcher refuses a value outside the range,
/// so one that does not parse here reads the default. Pure.
pub fn heap_dump_gzip_level_arg(config: &super::VmConfig) -> u8 {
    jvm_arg_value(config, "HeapDumpGzipLevel")
        .and_then(|text| text.parse::<u8>().ok())
        .filter(|level| *level <= 9)
        .unwrap_or(0)
}

/// `CRATONVM_JMX_CODE_CACHE_HOTSPOT_LAYOUT` (zt t30/nh, default ON, `=0`
/// kill; every collector, both modes): the code pools follow HotSpot's code
/// cache layout ([`code_cache_layout`]): G1 and Generational state each
/// heap's `init` (2555904 by default, as ZGC already did), an unsegmented
/// code cache lists one `CodeCache` pool, and `InitialCodeCacheSize` moves
/// the `init`s. `=0`: ZGC's fixed 2555904 per heap, G1 and Generational gci
/// i15/nh's 2 MiB on the non-profiled heap, three heaps whatever the options.
pub fn code_cache_hotspot_layout_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_CODE_CACHE_HOTSPOT_LAYOUT")
}

/// zt t30/nh: HotSpot's code cache layout for this VM's options
/// (`ReservedCodeCacheSize`, `InitialCodeCacheSize`, `SegmentedCodeCache`,
/// `NonNMethodCodeHeapSize`, `CICompilerCount`, and what makes compilation
/// tiered: `TieredCompilation`, `-Xint`, `TieredStopAtLevel`), sized for
/// `ActiveProcessorCount` or the processors this VM sees. Pure but for the
/// host's processor count.
pub fn code_cache_layout(
    config: &super::VmConfig,
) -> cratonvm_types::code_cache_layout::CodeCacheLayout {
    cratonvm_types::code_cache_layout::hotspot_code_cache_layout(&code_cache_args(config))
}

/// zki (page `zgc-zt23hunt4-*` item 4): the bytes of 2 MiB hugetlbfs pages
/// HotSpot 25's code cache takes from the pool under `-XX:+UseLargePages`
/// for this VM's options
/// ([`cratonvm_types::code_cache_layout::hotspot_code_cache_large_page_bytes`]),
/// `0` when it keeps small pages. Pure but for the host's processor count.
pub fn code_cache_large_page_bytes(config: &super::VmConfig) -> u64 {
    cratonvm_types::code_cache_layout::hotspot_code_cache_large_page_bytes(
        &code_cache_args(config),
        2 * 1024 * 1024,
    )
    .unwrap_or(0)
}

/// zt t30/nh: the code cache options of [`code_cache_layout`].
fn code_cache_args(config: &super::VmConfig) -> cratonvm_types::code_cache_layout::CodeCacheArgs {
    use cratonvm_types::code_cache_layout::{parse_hotspot_size, CodeCacheArgs};
    let size = |name: &str| jvm_arg_value(config, name).and_then(parse_hotspot_size);
    let interpreted = config.jvm_args.iter().any(|a| a == "-Xint");
    let stop_level = jvm_arg_value(config, "TieredStopAtLevel")
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or(4);
    let tiered = jvm_arg_bool(config, "TieredCompilation", true) && !interpreted && stop_level >= 4;
    let cpus = jvm_arg_value(config, "ActiveProcessorCount")
        .and_then(|t| t.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .or(config.container_effective_processors)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() as u32));
    let segmented = config.jvm_args.iter().rev().find_map(|a| match a.as_str() {
        "-XX:+SegmentedCodeCache" => Some(true),
        "-XX:-SegmentedCodeCache" => Some(false),
        _ => None,
    });
    CodeCacheArgs {
        reserved: size("ReservedCodeCacheSize"),
        initial: size("InitialCodeCacheSize"),
        segmented,
        non_nmethod: size("NonNMethodCodeHeapSize"),
        tiered,
        cpus,
        compiler_count: jvm_arg_value(config, "CICompilerCount").and_then(|t| t.parse().ok()),
    }
}

/// zt t28/mf: HotSpot's `PrintFlagsFinal` text of a `double` flag (`%f`).
pub fn double_flag_text(value: f64) -> String {
    format!("{value:.6}")
}

/// `CRATONVM_GC_THREADS_FLAG_ERGONOMIC` (zt t29/pt, default ON, `=0` kill;
/// every collector, both modes): the `ParallelGCThreads` / `ConcGCThreads`
/// rows answer HotSpot 25's ergonomic counts (see [`gc_thread_flag_rows`]).
pub fn gc_threads_flag_ergonomic_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_THREADS_FLAG_ERGONOMIC")
}

/// zt t29/pt (x10 item 30h): `[ParallelGCThreads, ConcGCThreads]` as
/// `(value, origin)` for `config` on a machine with `cpus` active
/// processors, as HotSpot 25 sets them. An explicit value is
/// `{command line}` on every collector. Otherwise:
///
/// * G1 (`G1Arguments::initialize`): `ParallelGCThreads` is
///   `WorkerPolicy::parallel_worker_threads()` (one per CPU up to 8, then
///   five eighths of the rest), written with `FLAG_SET_DEFAULT` -- a plain
///   assignment, so the origin stays `{default}`; `ConcGCThreads` is
///   `G1ConcurrentMark::scale_concurrent_worker_threads`,
///   `max((ParallelGCThreads + 2) / 4, 1)`, written with `FLAG_SET_ERGO`
///   (`{ergonomic}`). The parallel count is the one G1's evacuation pool
///   runs and its `Parallel Workers:` gc,init line prints
///   (`ergonomic_gc_worker_threads`).
/// * ZGC (`ZArguments::initialize`, both written with `FLAG_SET_DEFAULT`:
///   `{default}`): `ZHeuristics::nparallel_workers` (60 % of the CPUs rounded
///   up) and the global `nconcurrent_workers` maximum (25 %, before static
///   young/old pool splitting), each capped at 2 % of
///   `MaxHeapSize` in 2 MiB pages and at least one -- the functions the
///   heap's own pools use (`zgc_nparallel_workers` / `zgc_nconcurrent_workers`).
/// * Serial: neither is set; `0` / `{default}`.
///
/// Pure (the caller reads the CPU count).
pub fn gc_thread_flag_rows(config: &super::VmConfig, cpus: usize) -> [(usize, &'static str); 2] {
    let explicit = |field: Option<usize>, name: &str| {
        field.or_else(|| jvm_arg_value(config, name).and_then(|text| text.parse::<usize>().ok()))
    };
    let parallel = explicit(
        config
            .g1_parallel_gc_threads
            .or(config.zgc_parallel_gc_threads),
        "ParallelGCThreads",
    );
    let conc = explicit(config.zgc_conc_gc_threads, "ConcGCThreads");
    let cpus = cpus.max(1);
    let parallel_row = match parallel {
        Some(n) => (n, "{command line}"),
        None => match config.gc_algorithm {
            super::GcAlgorithm::G1 => (
                cratonvm_gc::g1::ergonomic_gc_worker_threads(cpus),
                "{default}",
            ),
            #[cfg(feature = "zgc")]
            super::GcAlgorithm::Zgc => (
                cratonvm_gc::zgc::zgc_nparallel_workers(cpus, config.max_heap_size),
                "{default}",
            ),
            #[allow(unreachable_patterns)]
            _ => (0, "{default}"),
        },
    };
    let conc_row = match conc {
        Some(0) if config.gc_algorithm == super::GcAlgorithm::G1 => (
            ((parallel_row.0 + 2) / 4).max(1), "{command line, ergonomic}"),
        Some(n) => (n, "{command line}"),
        None => match config.gc_algorithm {
            super::GcAlgorithm::G1 => (((parallel_row.0 + 2) / 4).max(1), "{ergonomic}"),
            #[cfg(feature = "zgc")]
            super::GcAlgorithm::Zgc => (
                cratonvm_gc::zgc::zgc_nconcurrent_workers(
                    cpus,
                    config.max_heap_size,
                    // This row is the global maximum before static pool splitting.
                    true,
                ),
                "{default}",
            ),
            #[allow(unreachable_patterns)]
            _ => (0, "{default}"),
        },
    };
    [parallel_row, conc_row]
}

/// HotSpot 25's ZArguments::initialize age budget at the default
/// ZYoungCompactionLimit (25%). Each age needs one small page per concurrent
/// worker and one shared medium page. Medium pages use a power of two capped
/// at 1/32 of the heap and 32 MiB, and are disabled at the 2 MiB small size.
/// https://github.com/openjdk/jdk25u/blob/master/src/hotspot/share/gc/z/zArguments.cpp
/// https://github.com/openjdk/jdk25u/blob/master/src/hotspot/share/gc/z/zHeuristics.cpp
fn zgc_default_max_tenuring_threshold(heap: usize, concurrent_workers: usize) -> u8 {
    const SMALL_PAGE: usize = 2 * 1024 * 1024;
    let clamped = (heap / 32).clamp(SMALL_PAGE, 16 * SMALL_PAGE);
    let medium = 1usize << (usize::BITS - 1 - clamped.leading_zeros());
    let medium = if medium > SMALL_PAGE { medium } else { 0 };
    let per_age = concurrent_workers.saturating_mul(SMALL_PAGE).saturating_add(medium);
    (heap / 4).div_ceil(per_age.max(1)).min(15) as u8
}

pub fn resolved_flags(config: &super::VmConfig) -> Vec<HotspotVmFlag> {
    // Each name is an option prefix: `-XX:<Name>=`, a whole `-XX:+<Name>` /
    // `-XX:-<Name>`, or an `-Xmx` / `-Xms` alias followed by its value.
    let given = |names: &[&str]| {
        config.jvm_args.iter().any(|a| names.iter().any(|n| a.starts_with(n)))
    };
    let origin = |names: &[&str], otherwise: &'static str| {
        if given(names) {
            "{command line}"
        } else {
            otherwise
        }
    };
    let bool_text = |b: bool| b.to_string();
    let serial_origin = if given(&["-XX:+UseSerialGC", "-XX:+UseGenerationalGC"]) {
        "{command line}"
    } else {
        "{ergonomic}"
    };
    // zt t24/x20 (item 30c): the collector this VM runs without its flag
    // given was chosen by the VM, as HotSpot's `GCConfig::select_gc_ergonomically`
    // chooses G1 (`FLAG_SET_ERGO_IF_DEFAULT`): `{ergonomic}`, so `jcmd VM.flags`
    // (non-default rows) names it as HotSpot's names `-XX:+UseG1GC`. It read
    // `{default}` for the default ZGC (and for G1 chosen without its flag), and
    // `VM.flags` named no collector at all. `CRATONVM_VM_FLAGS_ERGONOMIC_GC_ORIGIN=0`
    // (default ON): `{default}` again.
    let ergonomic_gc =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_VM_FLAGS_ERGONOMIC_GC_ORIGIN");
    let chosen_gc_origin = |selected: bool, names: &[&str]| {
        if given(names) {
            "{command line}"
        } else if selected && ergonomic_gc {
            "{ergonomic}"
        } else {
            "{default}"
        }
    };
    let g1_selected = config.gc_algorithm == super::GcAlgorithm::G1;
    let zgc_selected = {
        #[cfg(feature = "zgc")]
        {
            config.gc_algorithm == super::GcAlgorithm::Zgc
        }
        #[cfg(not(feature = "zgc"))]
        {
            false
        }
    };
    let initial = config.initial_heap_size.min(config.max_heap_size);
    let min_heap = crate::runtime::unified_logging::hotspot_min_heap_size(
        config.min_heap_size,
        initial,
    );
    let tenuring = &config.tenuring;
    let tlab = &config.tlab_flags;
    // Row `ob2_pf_cl` (HotSpot 25.0.3): a `MaxTenuringThreshold` below the
    // default 7 lowers `InitialTenuringThreshold` to it, `{ergonomic}`.
    let cpus = config.container_effective_processors.map(|n| n as usize)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let max_tenuring = tenuring.max_tenuring_threshold.unwrap_or_else(|| {
        if zgc_selected {
            zgc_default_max_tenuring_threshold(config.max_heap_size, gc_thread_flag_rows(config, cpus)[1].0)
        } else { 15 }
    });
    let (initial_tenuring, initial_tenuring_origin) = match tenuring.initial_tenuring_threshold {
        Some(v) => (v, "{command line}"),
        None if tenuring.max_tenuring_threshold.is_some_and(|max| max < 7) => (max_tenuring, "{ergonomic}"),
        None => (7, "{default}"),
    };
    let mut rows: Vec<(&str, &str, String, &str, &str)> = vec![
        (
            "size_t",
            "InitialHeapSize",
            initial.to_string(),
            "{product}",
            origin(&["-Xms", "-XX:InitialHeapSize="], "{ergonomic}"),
        ),
        (
            "uint",
            "InitialTenuringThreshold",
            initial_tenuring.to_string(),
            "{product}",
            initial_tenuring_origin,
        ),
        (
            "uintx",
            "MaxHeapFreeRatio",
            config.max_heap_free_ratio.unwrap_or(70).to_string(),
            "{manageable}",
            origin(&["-XX:MaxHeapFreeRatio="], "{default}"),
        ),
        (
            "size_t",
            "MaxHeapSize",
            config.max_heap_size.to_string(),
            "{product}",
            origin(&["-Xmx", "-XX:MaxHeapSize="], "{ergonomic}"),
        ),
        (
            "int",
            "MaxJavaStackTraceDepth",
            config.max_java_stack_trace_depth.to_string(),
            "{product}",
            origin(&["-XX:MaxJavaStackTraceDepth="], "{default}"),
        ),
        (
            "uint",
            "MaxTenuringThreshold",
            max_tenuring.to_string(),
            "{product}",
            origin(&["-XX:MaxTenuringThreshold="], "{default}"),
        ),
        (
            "uintx",
            "MinHeapFreeRatio",
            config.min_heap_free_ratio.unwrap_or(40).to_string(),
            "{manageable}",
            origin(&["-XX:MinHeapFreeRatio="], "{default}"),
        ),
        (
            "size_t",
            "MinHeapSize",
            min_heap.to_string(),
            "{product}",
            origin(&["-Xms", "-XX:MinHeapSize="], "{ergonomic}"),
        ),
        (
            "bool",
            "ResizeTLAB",
            bool_text(tlab.resize_tlab.unwrap_or(true)),
            "{product}",
            origin(&["-XX:+ResizeTLAB", "-XX:-ResizeTLAB"], "{default}"),
        ),
        (
            "intx",
            "SoftRefLRUPolicyMSPerMB",
            config.soft_ref_lru_policy_ms_per_mb.unwrap_or(1000).to_string(),
            "{product}",
            origin(&["-XX:SoftRefLRUPolicyMSPerMB="], "{default}"),
        ),
        (
            "uintx",
            "SurvivorRatio",
            config.survivor_ratio.unwrap_or(8).to_string(),
            "{product}",
            origin(&["-XX:SurvivorRatio="], "{default}"),
        ),
        (
            "uint",
            "TargetSurvivorRatio",
            tenuring.target_survivor_ratio.unwrap_or(50).to_string(),
            "{product}",
            origin(&["-XX:TargetSurvivorRatio="], "{default}"),
        ),
        ("bool", "UseG1GC", bool_text(g1_selected), "{product}", chosen_gc_origin(g1_selected, &["-XX:+UseG1GC"])),
        ("bool", "UseParallelGC", bool_text(false), "{product}", "{default}"),
        (
            "bool",
            "UseSerialGC",
            bool_text(config.gc_algorithm == super::GcAlgorithm::Generational),
            "{product}",
            if config.gc_algorithm == super::GcAlgorithm::Generational { serial_origin } else { "{default}" },
        ),
        (
            "bool",
            "UseTLAB",
            bool_text(tlab.use_tlab.unwrap_or(true)),
            "{product}",
            origin(&["-XX:+UseTLAB", "-XX:-UseTLAB"], "{default}"),
        ),
        ("bool", "UseZGC", bool_text(zgc_selected), "{product}", chosen_gc_origin(zgc_selected, &["-XX:+UseZGC"])),
    ];
    // These existing JFR facts also belong to the common snapshot: every
    // diagnostic reader must see the same configured value and origin.
    rows.extend([
        ("double", "G1ConcMarkStepDurationMillis",
         double_flag_text(jvm_arg_value(config, "G1ConcMarkStepDurationMillis")
             .and_then(|text| text.parse::<f64>().ok()).unwrap_or(10.0)),
         "{product}", origin(&["-XX:G1ConcMarkStepDurationMillis="], "{default}")),
        ("ccstr", "CompileCommandFile",
         jvm_arg_value(config, "CompileCommandFile").unwrap_or("").to_owned(),
         "{product}", origin(&["-XX:CompileCommandFile="], "{default}")),
    ]);
    // zt t29/pt (x10 item 30h, `CRATONVM_GC_THREADS_FLAG_ERGONOMIC`, default
    // ON): `ParallelGCThreads` / `ConcGCThreads` carry the counts HotSpot 25's
    // ergonomics set on this collector ([`gc_thread_flag_rows`]); they read
    // `0` / `{default}` on G1 and ZGC. `=0`: the old `ParallelGCThreads` row
    // (the explicit value or `0`, `{default}`) and no `ConcGCThreads` row.
    if gc_threads_flag_ergonomic_enabled() {
        let [parallel, conc] = gc_thread_flag_rows(config, cpus);
        for (name, (value, row_origin)) in
            [("ParallelGCThreads", parallel), ("ConcGCThreads", conc)]
        {
            rows.push(("uint", name, value.to_string(), "{product}", row_origin));
        }
    } else {
        rows.push((
            "uint",
            "ParallelGCThreads",
            config.g1_parallel_gc_threads.unwrap_or(0).to_string(),
            "{product}",
            origin(&["-XX:ParallelGCThreads="], "{default}"),
        ));
    }
    // Interpreter round i7 wave 136, lane L1b (`CRATONVM_VM_COMPRESSED_OOPS_MODE_HOTSPOT`,
    // parity, default ON, `=0` leaves both rows out): `UseCompressedOops` and
    // `ObjectAlignmentInBytes`, which `HotSpotDiagnosticMXBean.getVMOption`
    // callers read (Lucene's `RamUsageEstimator`, Elasticsearch's `JvmInfo`,
    // JOL) and which answered `IllegalArgumentException` (no such option).
    // MEASURED on HotSpot 25.0.3 (`-XX:+PrintFlagsFinal`, probe
    // `L1bW136DiagnosticVmOptions`): `{product lp64_product}`; `true
    // {ergonomic}` for a heap under the narrow-oop limit, `false {ergonomic}`
    // under ZGC, `false {default}` past the limit, `{command line}` when given;
    // `ObjectAlignmentInBytes = 8 {default}` (this VM aligns objects to 8).
    if config.vm_compressed_oops_mode_hotspot {
        let (coops, coops_origin) = hotspot_use_compressed_oops(config);
        rows.push((
            "bool",
            "UseCompressedOops",
            bool_text(coops),
            "{product lp64_product}",
            coops_origin,
        ));
        rows.push((
            "int",
            "ObjectAlignmentInBytes",
            "8".to_string(),
            "{product lp64_product}",
            "{default}",
        ));
    }
    // zt t26/x21 (`CRATONVM_HOTSPOT_FLAG_TABLE_ROWS`, default ON, `=0` kill;
    // pages `zgc-zt22hunt3-always-pre-touch-ignored-*` item 5 and
    // `zgc-zt13x10-review-residuals-*` item 30h): HotSpot flags this VM
    // honours per VM, with HotSpot 25's types, kinds and defaults
    // (`globals.hpp` / `gc_globals.hpp`, read), so `getVMOption`,
    // PrintFlagsFinal and `VM.flags` answer them as HotSpot does instead of
    // "does not exist" (`getVMOption("AlwaysPreTouch")` threw
    // `IllegalArgumentException`, rows `h4_V1` / `h4_V2`). `SoftMaxHeapSize`
    // is HotSpot's on every collector: `manageable`, set to `MaxHeapSize`
    // ergonomically when not given (rows `x20_mf*`: `getDiagnosticOptions`
    // lists it under `-XX:+UseSerialGC` too). A flag whose HotSpot value
    // depends on ergonomics this VM does not model (`UseLargePages`,
    // `UseTransparentHugePages`) is not added; the GC thread counts are
    // modelled since zt t29/pt (above).
    if hotspot_flag_table_rows_enabled() {
        // zt t28/sm: ZGC's ergonomic 90 % when the launcher decided one.
        let soft_max = config
            .zgc_soft_max_heap_size
            .or(config.zgc_ergonomic_soft_max_heap_size)
            .unwrap_or(config.max_heap_size)
            .min(config.max_heap_size);
        rows.extend([
            (
                "bool",
                "AlwaysPreTouch",
                bool_text(config.always_pre_touch),
                "{product}",
                origin(&["-XX:+AlwaysPreTouch", "-XX:-AlwaysPreTouch"], "{default}"),
            ),
            // j27 wave 7, lane T1EA (r18 row STR17-6): HotSpot 25's default
            // `CompactStrings = true {product}`, which this VM always runs
            // (`vm_object::pre_init_string_statics` sets `COMPACT_STRINGS`).
            // `getVMOption("CompactStrings")` threw `IllegalArgumentException`.
            // j27 wave 9 (W9VER): `-XX:-CompactStrings` is honoured
            // (`VmConfig::compact_strings_enabled`), so that arm reads `false`.
            (
                "bool",
                "CompactStrings",
                bool_text(config.compact_strings_enabled()),
                "{product}",
                origin(&["-XX:+CompactStrings", "-XX:-CompactStrings"], "{default}"),
            ),
            (
                "bool",
                "CrashOnOutOfMemoryError",
                bool_text(config.crash_on_out_of_memory_error),
                "{product}",
                origin(
                    &["-XX:+CrashOnOutOfMemoryError", "-XX:-CrashOnOutOfMemoryError"],
                    "{default}",
                ),
            ),
            (
                "bool",
                "DisableExplicitGC",
                bool_text(config.explicit_gc == super::ExplicitGcPolicy::Disabled),
                "{product}",
                origin(&["-XX:+DisableExplicitGC", "-XX:-DisableExplicitGC"], "{default}"),
            ),
            (
                "bool",
                "ExitOnOutOfMemoryError",
                bool_text(config.exit_on_out_of_memory_error),
                "{product}",
                origin(
                    &["-XX:+ExitOnOutOfMemoryError", "-XX:-ExitOnOutOfMemoryError"],
                    "{default}",
                ),
            ),
            (
                "bool",
                "ExplicitGCInvokesConcurrent",
                bool_text(config.explicit_gc_invokes_concurrent),
                "{product}",
                origin(
                    &["-XX:+ExplicitGCInvokesConcurrent", "-XX:-ExplicitGCInvokesConcurrent"],
                    "{default}",
                ),
            ),
            (
                "size_t",
                "SoftMaxHeapSize",
                soft_max.to_string(),
                "{manageable}",
                origin(&["-XX:SoftMaxHeapSize="], "{ergonomic}"),
            ),
            (
                "bool",
                "StackTraceInThrowable",
                bool_text(config.stack_trace_in_throwable),
                "{product}",
                origin(
                    &["-XX:+StackTraceInThrowable", "-XX:-StackTraceInThrowable"],
                    "{default}",
                ),
            ),
        ]);
    }
    // zt t27/lg1 (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`, default ON, `=0`
    // kill; page `zgc-zt13x10-review-residuals-*` item 30h): HotSpot 25's
    // manageable flags whose run-time effect this VM has, with HotSpot's
    // types and defaults, so `getDiagnosticOptions()` lists them and
    // `setVMOption` writes them (`HeapRealm::managed_flags`, read by each
    // consumer): `HeapDumpOnOutOfMemoryError` / `HeapDumpPath` (the
    // out-of-memory dump), `PrintClassHistogram` (the `SIGQUIT` histogram),
    // `ShowCodeDetailsInExceptionMessages` (the helpful NPE text); on ZGC
    // also `HeapDumpBeforeFullGC` / `HeapDumpAfterFullGC`, which have no
    // effect there on HotSpot either (ZGC runs no full collection). zt
    // t31/x10: the two full-collection dumps on G1 / Generational too, with
    // their effect (`CRATONVM_GC_FULL_GC_HEAP_DUMPS`, below).
    // zt t28/mf (row `lg1_mf_diag`): also `PrintConcurrentLocks` (the
    // `SIGQUIT` dump's `Locked ownable synchronizers` sections) and
    // `HeapDumpGzipLevel` (the out-of-memory dump gzip-framed above 0) on
    // every collector, and on ZGC (zt t31/x10: on every collector, below) the three manageable rows HotSpot lists
    // that have no effect there (`FullGCHeapDumpLimit`, `G1PeriodicGCInterval`,
    // `G1PeriodicGCSystemLoadThreshold`), with HotSpot 25's types.
    if manageable_effect_flags_enabled() {
        rows.extend([
            (
                "bool",
                "HeapDumpOnOutOfMemoryError",
                bool_text(config.heap_dump_on_oom),
                "{manageable}",
                origin(
                    &["-XX:+HeapDumpOnOutOfMemoryError", "-XX:-HeapDumpOnOutOfMemoryError"],
                    "{default}",
                ),
            ),
            (
                "ccstr",
                "HeapDumpPath",
                config.heap_dump_path.clone().unwrap_or_default(),
                "{manageable}",
                origin(&["-XX:HeapDumpPath="], "{default}"),
            ),
            (
                "bool",
                "PrintClassHistogram",
                bool_text(jvm_arg_bool(config, "PrintClassHistogram", false)),
                "{manageable}",
                origin(&["-XX:+PrintClassHistogram", "-XX:-PrintClassHistogram"], "{default}"),
            ),
            (
                "bool",
                "ShowCodeDetailsInExceptionMessages",
                bool_text(crate::runtime::env_cache::show_code_details(config)),
                "{manageable}",
                origin(
                    &[
                        "-XX:+ShowCodeDetailsInExceptionMessages",
                        "-XX:-ShowCodeDetailsInExceptionMessages",
                    ],
                    "{default}",
                ),
            ),
        ]);
        rows.extend([
            (
                "bool",
                "PrintConcurrentLocks",
                bool_text(jvm_arg_bool(config, "PrintConcurrentLocks", false)),
                "{manageable}",
                origin(&["-XX:+PrintConcurrentLocks", "-XX:-PrintConcurrentLocks"], "{default}"),
            ),
            (
                "int",
                "HeapDumpGzipLevel",
                heap_dump_gzip_level_arg(config).to_string(),
                "{manageable}",
                origin(&["-XX:HeapDumpGzipLevel="], "{default}"),
            ),
        ]);
        // zt t31/x10 (`CRATONVM_GC_FULL_GC_HEAP_DUMPS`, default ON, `=0`
        // kill; item 30h): HotSpot 25 lists these five on every collector
        // (row `x10_fd_flags_*`), and on G1 / Serial the two dumps and the
        // limit have their effect (`gc_and_alloc::full_gc_heap_dump`), as
        // `G1PeriodicGCInterval` / `G1PeriodicGCSystemLoadThreshold` have
        // theirs on G1 (`g1_periodic_gc`). Off: ZGC only, as before.
        if zgc_selected || full_gc_heap_dumps_enabled() {
            rows.extend([
                (
                    "uint",
                    "FullGCHeapDumpLimit",
                    jvm_arg_value(config, "FullGCHeapDumpLimit")
                        .and_then(|text| text.parse::<u32>().ok())
                        .unwrap_or(0)
                        .to_string(),
                    "{manageable}",
                    origin(&["-XX:FullGCHeapDumpLimit="], "{default}"),
                ),
                (
                    "uintx",
                    "G1PeriodicGCInterval",
                    g1_periodic_gc_interval_arg(config).to_string(),
                    "{manageable}",
                    origin(&["-XX:G1PeriodicGCInterval="], "{default}"),
                ),
                (
                    "double",
                    "G1PeriodicGCSystemLoadThreshold",
                    double_flag_text(g1_periodic_gc_load_threshold_arg(config)),
                    "{manageable}",
                    origin(&["-XX:G1PeriodicGCSystemLoadThreshold="], "{default}"),
                ),
            ]);
            for name in ["HeapDumpAfterFullGC", "HeapDumpBeforeFullGC"] {
                let plus = format!("-XX:+{name}");
                let minus = format!("-XX:-{name}");
                rows.push((
                    "bool",
                    name,
                    bool_text(jvm_arg_bool(config, name, false)),
                    "{manageable}",
                    origin(&[plus.as_str(), minus.as_str()], "{default}"),
                ));
            }
        }
    }
    rows.sort_by(|a, b| a.1.cmp(b.1));
    rows.into_iter().map(|(ty, name, value, kind, origin)|
        HotspotVmFlag { ty, name, value, kind, origin }).collect()
}

/// HotSpot 25's `UseCompressedOops` value and `PrintFlagsFinal` origin for
/// this VM (interpreter round i7 wave 136, lane L1b): the command line's
/// `-XX:±UseCompressedOops` (`{command line}`; `+` holds only where compressed
/// oops can be used), else `Arguments::set_use_compressed_oops`' ergonomics --
/// `true {ergonomic}` under the narrow-oop limit, `false {ergonomic}` under
/// ZGC (`ZArguments`), `false {default}` past the limit -- the same model as
/// `java.vm.compressedOopsMode` (`vm_properties::hotspot_compressed_oops_mode`).
/// A VM that really runs with narrow oops answers `true`.
fn hotspot_use_compressed_oops(config: &super::VmConfig) -> (bool, &'static str) {
    use cratonvm_types::vm_properties::{hotspot_compressed_oops_mode, CompressedOopsCollector};
    let collector = match config.gc_algorithm {
        super::GcAlgorithm::Generational => CompressedOopsCollector::Serial,
        super::GcAlgorithm::G1 => CompressedOopsCollector::G1,
        #[cfg(feature = "zgc")]
        super::GcAlgorithm::Zgc => CompressedOopsCollector::Z,
    };
    let fits =
        hotspot_compressed_oops_mode(config.max_heap_size as u64, collector, true).is_some();
    let explicit = config
        .jvm_args
        .iter()
        .filter_map(|a| match a.as_str() {
            "-XX:+UseCompressedOops" => Some(true),
            "-XX:-UseCompressedOops" => Some(false),
            _ => None,
        })
        .last();
    match explicit {
        Some(on) => (on && fits, "{command line}"),
        None if cratonvm_types::narrow_oop::narrow_oops_enabled() || fits => (true, "{ergonomic}"),
        None if collector == CompressedOopsCollector::Z => (false, "{ergonomic}"),
        None => (false, "{default}"),
    }
}

#[cfg(test)]
mod i136_l1b_tests {
    use super::resolved_flags;
    use crate::config::{GcAlgorithm, VmConfig};

    fn row(config: &VmConfig, name: &str) -> Option<(String, &'static str)> {
        resolved_flags(config)
            .into_iter()
            .find(|r| r.name == name)
            .map(|r| (r.value, r.origin))
    }

    /// Interpreter round i7 wave 136, lane L1b: `UseCompressedOops` and
    /// `ObjectAlignmentInBytes` as HotSpot 25.0.3's `PrintFlagsFinal` measured
    /// them (`L1bW136DiagnosticVmOptions`), and no rows with the parity switch
    /// off.
    #[test]
    fn the_compressed_oops_rows_follow_hotspots_ergonomics() {
        let mut config = VmConfig::default();
        config.vm_compressed_oops_mode_hotspot = true;
        config.gc_algorithm = GcAlgorithm::Generational;
        config.max_heap_size = 256 << 20;
        config.jvm_args = vec!["-XX:+UseGenerationalGC".to_string(), "-Xmx256m".to_string()];
        assert_eq!(row(&config, "UseCompressedOops"), Some(("true".to_string(), "{ergonomic}")));
        assert_eq!(row(&config, "ObjectAlignmentInBytes"), Some(("8".to_string(), "{default}")));
        config.jvm_args.push("-XX:-UseCompressedOops".to_string());
        assert_eq!(
            row(&config, "UseCompressedOops"),
            Some(("false".to_string(), "{command line}"))
        );
        config.jvm_args.pop();
        if !cratonvm_types::narrow_oop::narrow_oops_enabled() {
            config.max_heap_size = 40 << 30;
            assert_eq!(
                row(&config, "UseCompressedOops"),
                Some(("false".to_string(), "{default}"))
            );
        }
        config.vm_compressed_oops_mode_hotspot = false;
        assert_eq!(row(&config, "UseCompressedOops"), None);
        assert_eq!(row(&config, "ObjectAlignmentInBytes"), None);
    }
}

/// zt t26/x21 (`CRATONVM_HOTSPOT_FLAG_TABLE_ROWS`): the added rows carry
/// HotSpot's defaults and origins (rows `h4_V1` / `h4_V2`: `AlwaysPreTouch`
/// `true` / `VM_CREATION` and `false` / `DEFAULT`), `SoftMaxHeapSize` is a
/// manageable row on every collector (`MaxHeapSize`, `{ergonomic}`), and
/// `=0` drops them.
#[cfg(test)]
mod zt26x21_flag_table_rows_tests {
    use super::resolved_flags;
    use crate::config::{GcAlgorithm, VmConfig};

    const KEY: &str = "CRATONVM_HOTSPOT_FLAG_TABLE_ROWS";

    fn row(config: &VmConfig, name: &str) -> Option<(String, &'static str, &'static str)> {
        resolved_flags(config)
            .into_iter()
            .find(|row| row.name == name)
            .map(|row| (row.value, row.kind, row.origin))
    }

    #[test]
    fn zt26x21_always_pre_touch_and_soft_max_rows() {
        let mut config = VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            max_heap_size: 64 * 1024 * 1024,
            ..VmConfig::default()
        };
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                row(&config, "AlwaysPreTouch"),
                Some(("false".to_string(), "{product}", "{default}"))
            );
            assert_eq!(
                row(&config, "SoftMaxHeapSize"),
                Some(("67108864".to_string(), "{manageable}", "{ergonomic}"))
            );
            assert_eq!(
                row(&config, "StackTraceInThrowable"),
                Some(("true".to_string(), "{product}", "{default}"))
            );
            let names: Vec<&str> = resolved_flags(&config).iter().map(|r| r.name).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            assert_eq!(names, sorted, "still sorted by name");
        });
        config.always_pre_touch = true;
        config.zgc_soft_max_heap_size = Some(32 * 1024 * 1024);
        config.jvm_args = vec![
            "-XX:+AlwaysPreTouch".to_string(),
            "-XX:SoftMaxHeapSize=32m".to_string(),
        ];
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                row(&config, "AlwaysPreTouch"),
                Some(("true".to_string(), "{product}", "{command line}"))
            );
            assert_eq!(
                row(&config, "SoftMaxHeapSize"),
                Some(("33554432".to_string(), "{manageable}", "{command line}"))
            );
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert_eq!(row(&config, "AlwaysPreTouch"), None, "=0 kill");
            assert_eq!(row(&config, "SoftMaxHeapSize"), None, "=0 kill");
            assert!(row(&config, "MinHeapFreeRatio").is_some());
        });
    }
}

/// zt t27/lg1 (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`): the manageable rows
/// with an effect carry HotSpot's types and defaults, the command line's
/// values and origins, the full-collection dumps only on ZGC, and `=0`
/// drops them.
#[cfg(test)]
mod zt27lg1_manageable_effect_rows_tests {
    use super::{jvm_arg_bool, resolved_flags};
    use crate::config::{GcAlgorithm, VmConfig};

    const KEY: &str = concat!("CRATON", "VM_JMX_MANAGEABLE_EFFECT_FLAGS");

    fn row(config: &VmConfig, name: &str) -> Option<(&'static str, String, &'static str, &'static str)> {
        resolved_flags(config)
            .into_iter()
            .find(|row| row.name == name)
            .map(|row| (row.ty, row.value, row.kind, row.origin))
    }

    #[test]
    fn zt27lg1_effect_rows_follow_the_command_line() {
        let mut config = VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        };
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                row(&config, "HeapDumpOnOutOfMemoryError"),
                Some(("bool", "false".to_string(), "{manageable}", "{default}"))
            );
            assert_eq!(
                row(&config, "HeapDumpPath"),
                Some(("ccstr", String::new(), "{manageable}", "{default}"))
            );
            assert_eq!(
                row(&config, "PrintClassHistogram").map(|r| r.1),
                Some("false".to_string())
            );
            // zt t31/x10: on every collector (HotSpot lists it on Serial too).
            assert_eq!(
                row(&config, "HeapDumpBeforeFullGC"),
                Some(("bool", "false".to_string(), "{manageable}", "{default}"))
            );
            // zt t28/mf: modelled (the SIGQUIT locked-synchronizer sections).
            assert_eq!(
                row(&config, "PrintConcurrentLocks"),
                Some(("bool", "false".to_string(), "{manageable}", "{default}"))
            );
            assert_eq!(
                row(&config, "HeapDumpGzipLevel"),
                Some(("int", "0".to_string(), "{manageable}", "{default}"))
            );
            assert_eq!(
                row(&config, "G1PeriodicGCInterval").map(|r| r.1),
                Some("0".to_string())
            );
        });
        config.heap_dump_on_oom = true;
        config.heap_dump_path = Some("/tmp/d".to_string());
        config.jvm_args = vec![
            "-XX:+HeapDumpOnOutOfMemoryError".to_string(),
            "-XX:HeapDumpPath=/tmp/d".to_string(),
            "-XX:+PrintClassHistogram".to_string(),
            "-XX:-PrintClassHistogram".to_string(),
            "-XX:+PrintClassHistogram".to_string(),
        ];
        assert!(jvm_arg_bool(&config, "PrintClassHistogram", false), "the last one wins");
        assert!(!jvm_arg_bool(&config, "PrintClass", false), "a whole name only");
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                row(&config, "HeapDumpOnOutOfMemoryError").map(|r| (r.1, r.3)),
                Some(("true".to_string(), "{command line}"))
            );
            assert_eq!(
                row(&config, "HeapDumpPath").map(|r| (r.1, r.3)),
                Some(("/tmp/d".to_string(), "{command line}"))
            );
            assert_eq!(
                row(&config, "PrintClassHistogram").map(|r| (r.1, r.3)),
                Some(("true".to_string(), "{command line}"))
            );
        });
        #[cfg(feature = "zgc")]
        {
            config.gc_algorithm = GcAlgorithm::Zgc;
            cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
                assert_eq!(
                    row(&config, "HeapDumpAfterFullGC").map(|r| (r.0, r.1, r.2)),
                    Some(("bool", "false".to_string(), "{manageable}"))
                );
            });
        }
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(row(&config, "HeapDumpOnOutOfMemoryError").is_none(), "=0 kill");
            assert!(row(&config, "HeapDumpPath").is_none(), "=0 kill");
        });
    }
}

/// zt t24/x20 (`CRATONVM_VM_FLAGS_ERGONOMIC_GC_ORIGIN`): the running
/// collector's row without its flag is `{ergonomic}`, with it `{command line}`;
/// a collector not running stays `{default}`; `=0` restores `{default}`.
#[cfg(test)]
mod zt24x20_gc_origin_tests {
    use super::resolved_flags;
    use crate::config::{GcAlgorithm, VmConfig};

    fn origin_of(config: &VmConfig, name: &str) -> &'static str {
        resolved_flags(config)
            .into_iter()
            .find(|row| row.name == name)
            .map(|row| row.origin)
            .expect("a modelled row")
    }

    #[test]
    fn the_chosen_collector_is_ergonomic() {
        const KEY: &str = "CRATONVM_VM_FLAGS_ERGONOMIC_GC_ORIGIN";
        let mut config = VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        };
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(origin_of(&config, "UseG1GC"), "{ergonomic}");
            assert_eq!(origin_of(&config, "UseZGC"), "{default}");
            assert_eq!(origin_of(&config, "UseSerialGC"), "{default}");
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert_eq!(origin_of(&config, "UseG1GC"), "{default}", "=0 kill");
        });
        config.jvm_args = vec!["-XX:+UseG1GC".to_string()];
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(origin_of(&config, "UseG1GC"), "{command line}");
        });
    }
}

/// zt t30/nh: the code cache layout reads the `-XX:` options HotSpot sizes it
/// from (host rows `30nh_*`).
#[cfg(test)]
mod zt30nh_code_cache_layout_tests {
    use super::code_cache_layout;
    use crate::config::VmConfig;

    fn layout(args: &[&str]) -> cratonvm_types::code_cache_layout::CodeCacheLayout {
        let config = VmConfig {
            jvm_args: args.iter().map(|a| a.to_string()).collect(),
            container_effective_processors: Some(4),
            ..VmConfig::default()
        };
        code_cache_layout(&config)
    }

    #[test]
    fn zt30nh_options_shape_the_layout() {
        let d = layout(&[]);
        assert!(d.segmented);
        assert_eq!(d.inits, [2_555_904; 3]);
        assert!(!layout(&["-XX:+SegmentedCodeCache", "-XX:-SegmentedCodeCache"]).segmented);
        let small = layout(&["-XX:ReservedCodeCacheSize=64m"]);
        assert!(!small.segmented);
        assert_eq!((small.inits[0], small.reserved), (2_555_904, 64 << 20));
        assert!(layout(&["-XX:ReservedCodeCacheSize=300m"]).segmented);
        assert!(!layout(&["-XX:-TieredCompilation"]).segmented);
        assert!(!layout(&["-Xint"]).segmented);
        assert!(!layout(&["-XX:TieredStopAtLevel=1"]).segmented);
        assert!(layout(&["-XX:ReservedCodeCacheSize=64m", "-XX:+SegmentedCodeCache"]).segmented);
        #[cfg(not(windows))]
        assert_eq!(
            layout(&["-XX:InitialCodeCacheSize=8m"]).inits,
            [5_836_800, 8_388_608, 8_388_608]
        );
        assert_eq!(
            layout(&["-XX:InitialCodeCacheSize=8m", "-XX:ActiveProcessorCount=16"]).inits[1],
            8_388_608
        );
    }
}

/// zt t29/pt (x10 item 30h, `CRATONVM_GC_THREADS_FLAG_ERGONOMIC`): the GC
/// thread rows answer HotSpot 25's ergonomic counts and origins per
/// collector, an explicit value `{command line}`; `=0` restores the old
/// `ParallelGCThreads` row and drops `ConcGCThreads`.
#[cfg(test)]
mod zt29pt_gc_thread_rows_tests {
    use super::{gc_thread_flag_rows, resolved_flags};
    use crate::config::{GcAlgorithm, VmConfig};

    const KEY: &str = "CRATONVM_GC_THREADS_FLAG_ERGONOMIC";

    fn config(gc_algorithm: GcAlgorithm) -> VmConfig {
        VmConfig {
            gc_algorithm,
            max_heap_size: 4 * 1024 * 1024 * 1024,
            ..VmConfig::default()
        }
    }

    #[test]
    fn g1_parallel_is_default_and_conc_is_ergonomic() {
        let g1 = config(GcAlgorithm::G1);
        for (cpus, parallel, conc) in [(1, 1, 1), (4, 4, 1), (8, 8, 2), (16, 13, 3), (32, 23, 6)] {
            assert_eq!(
                gc_thread_flag_rows(&g1, cpus),
                [(parallel, "{default}"), (conc, "{ergonomic}")],
                "cpus={cpus}"
            );
        }
        let mut given = config(GcAlgorithm::G1);
        given.g1_parallel_gc_threads = Some(3);
        given.zgc_parallel_gc_threads = Some(3);
        given.jvm_args = vec!["-XX:ParallelGCThreads=3".to_string()];
        assert_eq!(
            gc_thread_flag_rows(&given, 8),
            [(3, "{command line}"), (1, "{ergonomic}")],
            "ConcGCThreads scales the given ParallelGCThreads"
        );
        given.zgc_conc_gc_threads = Some(2);
        assert_eq!(gc_thread_flag_rows(&given, 8)[1], (2, "{command line}"));
    }

    #[cfg(feature = "zgc")]
    #[test]
    fn zgc_counts_follow_its_heuristics_with_default_origin() {
        let mut zgc = config(GcAlgorithm::Zgc);
        assert_eq!(
            gc_thread_flag_rows(&zgc, 8),
            [(5, "{default}"), (2, "{default}")]
        );
        zgc.zgc_dynamic_gc_threads = Some(false);
        assert_eq!(gc_thread_flag_rows(&zgc, 8)[1], (2, "{default}"));
        zgc.max_heap_size = 64 * 1024 * 1024;
        assert_eq!(
            gc_thread_flag_rows(&zgc, 8),
            [(1, "{default}"), (1, "{default}")]
        );
        zgc.jvm_args = vec!["-XX:ParallelGCThreads=3".to_string()];
        zgc.zgc_parallel_gc_threads = Some(3);
        assert_eq!(gc_thread_flag_rows(&zgc, 8)[0], (3, "{command line}"));
    }

    #[test]
    fn serial_sets_neither() {
        let mut serial = config(GcAlgorithm::Generational);
        assert_eq!(
            gc_thread_flag_rows(&serial, 8),
            [(0, "{default}"), (0, "{default}")]
        );
        // A commanded zero (no `VmConfig` field takes ConcGCThreads=0) keeps its origin.
        serial.jvm_args = vec!["-XX:ConcGCThreads=0".to_string()];
        assert_eq!(gc_thread_flag_rows(&serial, 8)[1], (0, "{command line}"));
    }

    #[test]
    fn the_kill_word_restores_the_old_row() {
        let row = |config: &VmConfig, name: &str| {
            resolved_flags(config)
                .into_iter()
                .find(|row| row.name == name)
                .map(|row| (row.value, row.origin))
        };
        let g1 = config(GcAlgorithm::G1);
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            let (value, origin) = row(&g1, "ParallelGCThreads").expect("modelled");
            assert_ne!(value, "0");
            assert_eq!(origin, "{default}");
            assert_eq!(row(&g1, "ConcGCThreads").map(|r| r.1), Some("{ergonomic}"));
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert_eq!(
                row(&g1, "ParallelGCThreads"),
                Some(("0".to_string(), "{default}"))
            );
            assert!(row(&g1, "ConcGCThreads").is_none(), "=0 kill");
        });
    }
}

#[cfg(test)]
mod gcj_concurrent_flag_tests {
    use super::*;

    #[test]
    fn g1_concurrent_flags_follow_parallel_and_retain_zero_ergonomics() {
        let mut config = crate::config::VmConfig::default();
        config.gc_algorithm = crate::config::GcAlgorithm::G1;
        config.g1_parallel_gc_threads = Some(2);
        config.jvm_args = vec!["-XX:+UseG1GC".into(), "-XX:ParallelGCThreads=2".into()];
        let row = resolved_flags(&config).into_iter().find(|row| row.name == "ConcGCThreads").unwrap();
        assert_eq!((row.value.as_str(), row.origin), ("1", "{ergonomic}"));
        config.zgc_conc_gc_threads = Some(0);
        config.jvm_args.push("-XX:ConcGCThreads=0".into());
        let row = resolved_flags(&config).into_iter().find(|row| row.name == "ConcGCThreads").unwrap();
        assert_eq!((row.value.as_str(), row.origin), ("1", "{command line, ergonomic}"));
        assert_eq!((row.jmm_origin(), row.jmx_origin(), row.jfr_origin()), (6, "ERGONOMIC", "Ergonomic"));
        config.zgc_conc_gc_threads = Some(3);
        *config.jvm_args.last_mut().unwrap() = "-XX:ConcGCThreads=3".into();
        let row = resolved_flags(&config).into_iter().find(|row| row.name == "ConcGCThreads").unwrap();
        assert_eq!((row.value.as_str(), row.origin), ("3", "{command line}"));
    }
    #[test]
    fn zgc_default_age_follows_captured_heap_and_worker_budgets() {
        const MIB: usize = 1024 * 1024;
        for (heap, workers, expected) in [
            (64, 1, 8), (64, 2, 4), (128, 1, 6), (128, 2, 4),
            (256, 1, 7), (256, 2, 6), (512, 1, 8), (512, 2, 7),
            (512, 5, 5), (1024, 1, 8), (1024, 2, 8), (1024, 8, 6),
            (2048, 1, 15), (2048, 2, 15), (2048, 8, 11),
        ] {
            assert_eq!(zgc_default_max_tenuring_threshold(heap * MIB, workers), expected,
                "heap={heap} MiB workers={workers}");
        }
    }

    #[cfg(feature = "zgc")]
    #[test]
    fn zgc_derived_maximum_preserves_initial_age_and_explicit_clamps() {
        let mut config = crate::config::VmConfig::default();
        config.gc_algorithm = crate::config::GcAlgorithm::Zgc;
        config.max_heap_size = 256 * 1024 * 1024;
        config.zgc_conc_gc_threads = Some(1);
        let get = |config: &crate::config::VmConfig, name| resolved_flags(config)
            .into_iter().find(|row| row.name == name).unwrap();
        assert_eq!(get(&config, "MaxTenuringThreshold").value, "7");
        config.zgc_conc_gc_threads = Some(2);
        assert_eq!(get(&config, "MaxTenuringThreshold").value, "6");
        assert_eq!(get(&config, "InitialTenuringThreshold").value, "7");
        config.tenuring.max_tenuring_threshold = Some(4);
        let row = get(&config, "InitialTenuringThreshold");
        assert_eq!((row.value.as_str(), row.origin), ("4", "{ergonomic}"));
    }

}

#[cfg(test)]
mod gcj_flag_kinds_tests {
    use super::resolved_flags;
    #[test]
    fn configured_string_and_double_facts_are_shared_rows() {
        let mut config = crate::config::VmConfig::default();
        config.heap_dump_path = Some("gcj output".into());
        config.jvm_args = vec!["-XX:G1ConcMarkStepDurationMillis=2.5".into(),
            "-XX:CompileCommandFile=".into(), "-XX:HeapDumpPath=gcj output".into()];
        let rows = resolved_flags(&config);
        let row = |name| rows.iter().find(|row| row.name == name).unwrap();
        assert_eq!((row("G1ConcMarkStepDurationMillis").ty,
            row("G1ConcMarkStepDurationMillis").value.as_str(),
            row("G1ConcMarkStepDurationMillis").origin),
            ("double", "2.500000", "{command line}"));
        assert_eq!((row("CompileCommandFile").value.as_str(), row("CompileCommandFile").origin),
            ("", "{command line}"));
        assert_eq!(row("HeapDumpPath").value, "gcj output");
    }
}

/// j27 wave 7, lane T1EA (r18 row STR17-6): `CompactStrings` is a modelled
/// row with HotSpot 25's default (`getVMOption("CompactStrings")` answered
/// "does not exist"), under the flag-table switch, and its value is the VM's
/// own: `-XX:-CompactStrings` reads `false` (`{command line}`) since j27
/// wave 9 (W9VER) honours it.
#[cfg(test)]
mod j27_compact_strings_row_tests {
    use super::resolved_flags;
    use crate::config::VmConfig;

    const KEY: &str = "CRATONVM_HOTSPOT_FLAG_TABLE_ROWS";

    fn row(config: &VmConfig) -> Option<(String, &'static str, &'static str, &'static str)> {
        resolved_flags(config)
            .into_iter()
            .find(|row| row.name == "CompactStrings")
            .map(|row| (row.value, row.ty, row.kind, row.origin))
    }

    #[test]
    fn compact_strings_reads_hotspots_default_and_the_vms_own_value() {
        let mut config = VmConfig::default();
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                row(&config),
                Some(("true".to_string(), "bool", "{product}", "{default}"))
            );
        });
        config.jvm_args = vec!["-XX:-CompactStrings".to_string()];
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                row(&config),
                Some(("false".to_string(), "bool", "{product}", "{command line}"))
            );
        });
        config.jvm_args = vec!["-XX:-CompactStrings".to_string(), "-XX:+CompactStrings".to_string()];
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert_eq!(
                row(&config),
                Some(("true".to_string(), "bool", "{product}", "{command line}"))
            );
        });
        config.jvm_args = vec!["-XX:-CompactStrings".to_string()];
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert_eq!(row(&config), None, "=0 kill");
        });
    }
}
