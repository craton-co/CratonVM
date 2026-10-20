// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! LOST-FRAME CENSUS (JIT round 16 wave 8, lane trace; proposal TR15-5 of
//! `docs/internal/jit-proposals/jit-r15-trace-proposals-RETIRED-20261003.md`, first step).
//!
//! # What it finds
//!
//! Every live registered native that SERVES a method the JDK image gives a
//! bytecode body (`Code`, neither `ACC_NATIVE` nor abstract). Such a native
//! has no frame of its own, so against HotSpot it can lose frames two ways:
//!
//! * `top`: the body raises (`athrow` in it). An exception the native builds
//!   itself lists none of the frames the body (and what it calls to build the
//!   exception) would have run. `String.charAt` was such a row
//!   (`r16-throwpath-cached-charat-sioobe-has-no-jdk-frames-FIXED-20261001.md`), as
//!   were the `Preconditions` checks under `--compatible`.
//! * `middle`: the body calls Java (`invoke*` in it). When the native calls
//!   Java too and that call raises, the served method's own frame is missing
//!   from the middle of the trace.
//!
//! A `leaf` (no `athrow`, no call) loses nothing that a probe would see beyond
//! an implicit exception's top frame. Every stand-in row the throwable lanes
//! fixed so far was found by a probe; this turns the work list-driven. It is a
//! candidate list, not a gate: the native may raise through the body already
//! (`preconditions::raise_through_bytecode`) or never raise at all.
//!
//! Both modes are listed: `--compatible` (the default registry) and
//! `--jdk-only` (refused rows never land). The FOCUS classes (TR15-5's first
//! step: `java/lang/Class`, `java/lang/Thread`, `java/lang/ClassLoader`, plus
//! the `String` family this wave fixed) print one line per row; every other
//! class prints a per-class count.
//!
//! # Running it
//!
//! ```text
//! CRATONVM_JDK25_HOME=<jdk 25 home> cargo test -p cratonvm-native-builtins \
//!     --test r16_trace_lost_frame_census -- --ignored --nocapture
//! ```
//!
//! (`CRATONVM_JAVA_HOME` and `JAVA_HOME` are tried after it; the image read is
//! `<home>/lib/modules`, as in `r15_shadow_thin_wrapper_census.rs`.) Not yet
//! ranked by use: the Spring census that TR15-5 names as the ranking is not in
//! this tree; `--dump-native-registry` invocation counts on a workload are
//! the stand-in.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_reader::attribute::Attribute;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::{read_class, ClassFile, JImageReader};
use cratonvm_types::compat::CompatibilityMode;

/// The one model of `vm_init`'s real-JDK boot path, shared with the other
/// registry gates.
#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

const OP_ATHROW: u8 = 0xbf;

/// The classes whose rows print one line each.
const FOCUS: &[&str] = &[
    "java/lang/Class",
    "java/lang/Thread",
    "java/lang/ClassLoader",
    "java/lang/String",
    "java/lang/StringLatin1",
    "java/lang/StringUTF16",
    "java/lang/AbstractStringBuilder",
    "java/lang/StringBuilder",
    "jdk/internal/util/Preconditions",
];

/// Length of the instruction at `pc` (JVMS 6.5), `None` past the end or for a
/// malformed switch.
fn insn_len(code: &[u8], pc: usize) -> Option<usize> {
    let op = *code.get(pc)?;
    let read_i32 = |at: usize| -> Option<i64> {
        let b = code.get(at..at + 4)?;
        Some(i64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]])))
    };
    // Switch operands start at the next 4-byte boundary after the opcode.
    let pad = (4 - (pc + 1) % 4) % 4;
    let len = match op {
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xa8
        | 0xb2..=0xb8
        | 0xbb
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        0xc5 => 4,
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        // wide: `wide iinc` carries two 2-byte operands, every other one one.
        0xc4 => {
            if *code.get(pc + 1)? == 0x84 {
                6
            } else {
                4
            }
        }
        // tableswitch: default, low, high, then high - low + 1 offsets.
        0xaa => {
            let at = pc + 1 + pad;
            let (low, high) = (read_i32(at + 4)?, read_i32(at + 8)?);
            if high < low {
                return None;
            }
            1 + pad + 12 + 4 * usize::try_from(high - low + 1).ok()?
        }
        // lookupswitch: default, npairs, then npairs (match, offset) pairs.
        0xab => {
            let at = pc + 1 + pad;
            let pairs = usize::try_from(read_i32(at + 4)?).ok()?;
            1 + pad + 8 + 8 * pairs
        }
        _ => 1,
    };
    Some(len)
}

/// `(athrow count, invoke count)` of `code`, `None` if it does not decode.
fn raise_and_call_counts(code: &[u8]) -> Option<(usize, usize)> {
    let (mut athrows, mut invokes) = (0usize, 0usize);
    let mut pc = 0usize;
    while pc < code.len() {
        match code[pc] {
            OP_ATHROW => athrows += 1,
            0xb6..=0xba => invokes += 1,
            _ => {}
        }
        pc += insn_len(code, pc)?;
    }
    (pc == code.len()).then_some((athrows, invokes))
}

fn code_bytes(cf: &ClassFile, m: &ClassFileMethod) -> Option<Vec<u8>> {
    m.attributes.iter().find_map(|a| {
        let attr = a.decoded_or_decode(&cf.constant_pool)?;
        match &*attr {
            Attribute::Code(code) => Some(code.code.as_bytes().to_vec()),
            _ => None,
        }
    })
}

/// The classes of one `lib/modules` image, parsed on demand (the
/// thin-wrapper census's reader).
struct Image {
    reader: JImageReader,
    paths: HashMap<String, String>,
    cache: HashMap<String, Option<ClassFile>>,
}

impl Image {
    fn open(path: &Path) -> Option<Image> {
        let reader = JImageReader::open(path).ok()?;
        let mut paths = HashMap::new();
        for (full, _, _) in reader.iter_entries().ok()? {
            let Some(rest) = full.strip_prefix('/') else {
                continue;
            };
            let Some((_module, in_module)) = rest.split_once('/') else {
                continue;
            };
            if let Some(name) = in_module.strip_suffix(".class") {
                paths.insert(name.to_string(), full.clone());
            }
        }
        Some(Image {
            reader,
            paths,
            cache: HashMap::new(),
        })
    }

    fn class(&mut self, name: &str) -> Option<&ClassFile> {
        if !self.cache.contains_key(name) {
            let parsed = self
                .paths
                .get(name)
                .and_then(|p| self.reader.find_resource(p).ok().flatten())
                .and_then(|bytes| read_class(&bytes).ok());
            self.cache.insert(name.to_string(), parsed);
        }
        self.cache.get(name).and_then(|c| c.as_ref())
    }

    /// The body that serves `(name, descriptor)` on `class` (the class itself,
    /// then its superclass chain): `(declaring class, counts)`, or `None` when
    /// the declaration is native, abstract, absent or undecodable.
    fn served_body(
        &mut self,
        class: &str,
        name: &str,
        descriptor: &str,
    ) -> Option<(String, (usize, usize))> {
        let mut cur = class.to_string();
        for _ in 0..32 {
            let cf = self.class(&cur)?;
            if let Some(m) = cf.find_method(name, descriptor) {
                if m.is_native() || m.is_abstract() {
                    return None;
                }
                let counts = raise_and_call_counts(&code_bytes(cf, m)?)?;
                return Some((cur, counts));
            }
            cur = cf.super_class.as_deref()?.to_string();
        }
        None
    }
}

fn jdk_image() -> Option<PathBuf> {
    ["CRATONVM_JDK25_HOME", "CRATONVM_JAVA_HOME", "JAVA_HOME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|home| PathBuf::from(home).join("lib").join("modules"))
        .find(|p| p.is_file())
}

fn boot_registry(mode: Option<CompatibilityMode>) -> NativeMethodRegistry {
    let mut registry = NativeMethodRegistry::new();
    if let Some(mode) = mode {
        // Before registration, as `vm_init` does: refused rows never land.
        registry.set_compatibility_mode(mode);
    }
    boot_path::vm_init_real_jdk_boot_path(&mut registry);
    registry
}

fn verdict(athrows: usize, invokes: usize) -> &'static str {
    if athrows > 0 {
        "top"
    } else if invokes > 0 {
        "middle"
    } else {
        "leaf"
    }
}

#[test]
#[ignore = "reads a JDK 25 image; set CRATONVM_JDK25_HOME and run with --ignored"]
fn lost_frame_census() {
    let Some(modules) = jdk_image() else {
        println!(
            "lost-frame census: no <home>/lib/modules under CRATONVM_JDK25_HOME, \
             CRATONVM_JAVA_HOME or JAVA_HOME; nothing measured"
        );
        return;
    };
    let mut image = Image::open(&modules)
        .unwrap_or_else(|| panic!("{} is not a readable jimage", modules.display()));
    for (mode_name, mode) in [
        ("compatible", None),
        ("jdk-only", Some(CompatibilityMode::JdkOnly)),
    ] {
        let registry = boot_registry(mode);
        // Last registration wins, as dispatch does.
        let mut live: BTreeMap<(String, String, String), NativeKind> = BTreeMap::new();
        for (c, m, d, k) in registry.dump_registrations() {
            live.insert((c.to_string(), m.to_string(), d.to_string()), k);
        }
        let mut per_class: BTreeMap<String, [usize; 3]> = BTreeMap::new();
        let mut focus_rows = 0usize;
        for ((c, m, d), kind) in &live {
            let Some((declared_on, (athrows, invokes))) = image.served_body(c, m, d) else {
                continue;
            };
            let v = verdict(athrows, invokes);
            let slot = match v {
                "top" => 0,
                "middle" => 1,
                _ => 2,
            };
            per_class.entry(c.clone()).or_default()[slot] += 1;
            if FOCUS.contains(&c.as_str()) {
                focus_rows += 1;
                println!(
                    "lost-frames\t{mode_name}\t{v}\t{kind:?}\t{c}.{m}{d}\t\
                     declared-on={declared_on}\tathrow={athrows}\tinvokes={invokes}"
                );
            }
        }
        let (mut top, mut middle, mut leaf) = (0usize, 0usize, 0usize);
        for (class, [t, mi, l]) in &per_class {
            top += t;
            middle += mi;
            leaf += l;
            if !FOCUS.contains(&class.as_str()) && t + mi > 0 {
                println!("lost-frames-class\t{mode_name}\t{class}\ttop={t}\tmiddle={mi}\tleaf={l}");
            }
        }
        println!(
            "lost-frame census ({mode_name}): {} live rows, {} serve a JDK body: \
             top={top} middle={middle} leaf={leaf}; {focus_rows} rows on the focus classes",
            live.len(),
            top + middle + leaf
        );
        // A served `String` body exists in both modes (`String.length` at
        // least); zero means the image read or the replay broke.
        assert!(
            focus_rows > 0,
            "no served focus row in {mode_name}: the image read or the replay broke"
        );
    }
}

/// The walker on hand-read bodies, so the census cannot pass by decoding
/// nothing.
#[test]
fn the_walker_counts_raises_and_calls() {
    // Preconditions.checkIndex(IILBiFunction)I (JDK 25, javap -c):
    //   0: iload_0; 1: iflt 9; 4: iload_0; 5: iload_1; 6: if_icmplt 16;
    //   9: aload_2; iload_0; iload_1; 12: invokestatic #95 outOfBoundsCheckIndex;
    //   15: athrow; 16: iload_0; 17: ireturn
    let check_index = [
        0x1a, 0x9b, 0x00, 0x08, 0x1a, 0x1b, 0xa1, 0x00, 0x0a, 0x2c, 0x1a, 0x1b, 0xb8, 0x00, 0x5f,
        0xbf, 0x1a, 0xac,
    ];
    assert_eq!(raise_and_call_counts(&check_index), Some((1, 1)));
    assert_eq!(verdict(1, 1), "top");
    // A getter: aload_0; getfield; areturn.
    assert_eq!(
        raise_and_call_counts(&[0x2a, 0xb4, 0x00, 0x02, 0xb0]),
        Some((0, 0))
    );
    assert_eq!(verdict(0, 0), "leaf");
    // invokeinterface (5 bytes) then invokedynamic (5 bytes).
    let calls = [
        0x2a, 0xb9, 0x00, 0x03, 0x01, 0x00, 0xba, 0x00, 0x04, 0x00, 0x00, 0xb0,
    ];
    assert_eq!(raise_and_call_counts(&calls), Some((0, 2)));
    assert_eq!(verdict(0, 2), "middle");
    // tableswitch at pc 1: 2 bytes of padding, default, low 0, high 1, two
    // offsets, then athrow.
    let mut table = vec![0x1a, 0xaa, 0, 0];
    for word in [0i32, 0, 1, 0, 0] {
        table.extend_from_slice(&word.to_be_bytes());
    }
    table.push(0xbf);
    assert_eq!(insn_len(&table, 1), Some(1 + 2 + 12 + 8));
    assert_eq!(raise_and_call_counts(&table), Some((1, 0)));
    // lookupswitch at pc 0: 3 bytes of padding, default, one pair, return.
    let mut lookup = vec![0xab, 0, 0, 0];
    for word in [0i32, 1, 7, 0] {
        lookup.extend_from_slice(&word.to_be_bytes());
    }
    lookup.push(0xb1);
    assert_eq!(insn_len(&lookup, 0), Some(1 + 3 + 8 + 8));
    assert_eq!(raise_and_call_counts(&lookup), Some((0, 0)));
    // wide iinc is six bytes, wide iload four.
    assert_eq!(insn_len(&[0xc4, 0x84, 0, 1, 0, 1], 0), Some(6));
    assert_eq!(insn_len(&[0xc4, 0x15, 0, 1], 0), Some(4));
    // A body that runs off its end does not decode.
    assert_eq!(raise_and_call_counts(&[0x2a, 0xb6, 0x00]), None);
}
