// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! THIN-WRAPPER CENSUS (round 15 wave 1, lane shadow; proposal CA6-1 of
//! `docs/internal/jit-proposals/jit-r14-compat6-proposals-RETIRED-20260930.md`).
//!
//! # What it finds
//!
//! Every live `--jdk-only` `Bridge` row whose image method has a `Code`
//! attribute of the shape
//!
//! ```text
//!   (xload n | constant | checkcast)*  invoke*  [checkcast]  xreturn
//! ```
//!
//! -- one call and nothing else. `Array.newInstance` was such a row (one
//! `invokestatic newArray`), and nobody knew it was Java until a trace probe
//! showed its frame. For each hit the census reports what the one call
//! reaches: a registered native (and its kind), an `ACC_NATIVE` method, a Java
//! body, or an abstract declaration. A wrapper whose target is a LIVE native
//! is the cheapest retirement there is: no state, no carrier, and the answer
//! cannot change unless the two natives disagree. It is a candidate list, not a
//! gate; each retirement still needs its own screen (see
//! `r13w7-shadow-jdk-only-bridges-over-bytecode-census-CLOSED-20261006.md`).
//!
//! # Why the jimage and not the jmod
//!
//! CA6-1 said "read the `java.base` jmod". A stock JDK 25 need not ship
//! `jmods/` at all (JEP 493 builds; the Temurin 25.0.3 image on the round-15
//! Windows host has none), but every image has `lib/modules`, which
//! `cratonvm_reader::JImageReader` already reads. The census covers every
//! module in it, not only `java.base`.
//!
//! # Running it
//!
//! ```text
//! CRATONVM_JDK25_HOME=<jdk 25 home> cargo test -p cratonvm-native-builtins \
//!     --test r15_shadow_thin_wrapper_census -- --ignored --nocapture
//! ```
//!
//! (`CRATONVM_JAVA_HOME` and `JAVA_HOME` are tried after it.) Output: one
//! `thin-wrapper` line per hit, tab-separated, then a summary. The registry is
//! the shared real-JDK boot-path replay (`common/vm_init_boot_path.rs`), which
//! does not set `real_jdk(true)`: a registrar that branches on it is counted in
//! its synthetic-JDK shape here (r13w9 measurement note). Confirm a candidate
//! with `--dump-native-registry` on a real VM before retiring it.
//!
//! # Tiers (the `tier` column)
//!
//! * `A-acc-bridge`: a javac `ACC_BRIDGE` method whose one call reaches a live
//!   native. No source-level logic at all.
//! * `B-acc-native`: the call reaches an `ACC_NATIVE` image method with a live
//!   native (the `Array.newInstance` species).
//! * `C-live-native`: the call reaches another live native.
//! * `D-java`: the call reaches Java (or an abstract method) with no native:
//!   retiring the row runs a deeper Java body, which is not zero-risk.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_reader::attribute::Attribute;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::{read_class, ClassFile, JImageReader};
use cratonvm_types::compat::CompatibilityMode;

/// The one model of `vm_init`'s real-JDK boot path, shared with the other
/// registry gates.
#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

const OP_CHECKCAST: u8 = 0xc0;
const OP_INVOKEVIRTUAL: u8 = 0xb6;
const OP_INVOKESPECIAL: u8 = 0xb7;
const OP_INVOKESTATIC: u8 = 0xb8;
const OP_INVOKEINTERFACE: u8 = 0xb9;

/// The one call a thin wrapper makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThinCall {
    /// `invokevirtual` / `invokespecial` / `invokestatic` / `invokeinterface`.
    opcode: u8,
    /// Constant-pool index of the `Methodref` / `InterfaceMethodref`.
    cp_index: u16,
    /// The loads are exactly the wrapper's own receiver and parameters, in
    /// order, with no constant in between (a pure forward).
    forwards_exactly: bool,
}

/// Local-variable slots of `descriptor`'s parameters, receiver first unless
/// `is_static`. `None` for a malformed descriptor.
fn arg_slots(descriptor: &str, is_static: bool) -> Option<Vec<u16>> {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut slots = Vec::new();
    let mut next: u16 = 0;
    if !is_static {
        slots.push(0);
        next = 1;
    }
    let mut i = 1;
    while *bytes.get(i)? != b')' {
        slots.push(next);
        match bytes[i] {
            b'J' | b'D' => {
                next += 2;
                i += 1;
            }
            _ => {
                next += 1;
                while *bytes.get(i)? == b'[' {
                    i += 1;
                }
                if bytes[i] == b'L' {
                    i += descriptor[i..].find(';')?;
                }
                i += 1;
            }
        }
    }
    Some(slots)
}

/// Is `code` a thin wrapper: loads of locals or small constants (each
/// optionally followed by a `checkcast`, as javac's generic bridges cast their
/// arguments), exactly one invoke, an optional `checkcast`, and a return as
/// the last byte? `wide`, `ldc`, field access, branches and a second call all
/// answer `None`.
fn match_thin_wrapper(code: &[u8], is_static: bool, descriptor: &str) -> Option<ThinCall> {
    let mut i = 0usize;
    let mut loads: Vec<Option<u16>> = Vec::new();
    loop {
        let op = *code.get(i)?;
        match op {
            // iload, lload, fload, dload, aload <u8>
            0x15..=0x19 => {
                loads.push(Some(u16::from(*code.get(i + 1)?)));
                i += 2;
            }
            // xload_<n>, four per type starting at iload_0
            0x1a..=0x2d => {
                loads.push(Some(u16::from((op - 0x1a) % 4)));
                i += 1;
            }
            // aconst_null .. dconst_1
            0x01..=0x0f => {
                loads.push(None);
                i += 1;
            }
            // bipush <s8>
            0x10 => {
                loads.push(None);
                i += 2;
            }
            // sipush <s16>
            0x11 => {
                loads.push(None);
                i += 3;
            }
            OP_CHECKCAST if !loads.is_empty() => i += 3,
            _ => break,
        }
    }
    let opcode = *code.get(i)?;
    let width = match opcode {
        OP_INVOKEVIRTUAL | OP_INVOKESPECIAL | OP_INVOKESTATIC => 3,
        OP_INVOKEINTERFACE => 5,
        _ => return None,
    };
    let cp_index = u16::from_be_bytes([*code.get(i + 1)?, *code.get(i + 2)?]);
    i += width;
    if code.get(i) == Some(&OP_CHECKCAST) {
        i += 3;
    }
    // ireturn .. return, and nothing after it.
    if i + 1 != code.len() || !(0xac..=0xb1).contains(code.get(i)?) {
        return None;
    }
    let own: Vec<Option<u16>> = arg_slots(descriptor, is_static)?
        .into_iter()
        .map(Some)
        .collect();
    Some(ThinCall {
        opcode,
        cp_index,
        forwards_exactly: loads == own,
    })
}

/// `(owner, name, descriptor)` of a `Methodref` / `InterfaceMethodref`.
fn method_ref(cp: &ConstantPool, index: u16) -> Option<(String, String, String)> {
    let (class_index, name_and_type_index) = match cp.get(index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner = cp.get_class_name(class_index)?;
    let (name, descriptor) = cp.get_name_and_type(name_and_type_index)?;
    Some((owner.to_string(), name.to_string(), descriptor.to_string()))
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

/// The classes of one `lib/modules` image, parsed on demand.
struct Image {
    reader: JImageReader,
    /// Internal class name -> full jimage path (`/module/pkg/Name.class`).
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

    /// The class that declares `(name, descriptor)` on `class`'s superclass
    /// chain (the class itself first), with that declaration's facts.
    fn declaration(&mut self, class: &str, name: &str, descriptor: &str) -> Option<Declared> {
        let mut cur = class.to_string();
        for _ in 0..32 {
            let cf = self.class(&cur)?;
            if let Some(m) = cf.find_method(name, descriptor) {
                let is_static = m.is_static();
                let thin = code_bytes(cf, m)
                    .and_then(|code| match_thin_wrapper(&code, is_static, descriptor))
                    .and_then(|t| method_ref(&cf.constant_pool, t.cp_index).map(|r| (t, r)));
                return Some(Declared {
                    on: cur,
                    is_native: m.is_native(),
                    is_abstract: m.is_abstract(),
                    is_acc_bridge: m.is_bridge(),
                    thin,
                });
            }
            cur = cf.super_class.as_deref()?.to_string();
        }
        None
    }
}

struct Declared {
    on: String,
    is_native: bool,
    is_abstract: bool,
    is_acc_bridge: bool,
    thin: Option<(ThinCall, (String, String, String))>,
}

fn jdk_image() -> Option<PathBuf> {
    ["CRATONVM_JDK25_HOME", "CRATONVM_JAVA_HOME", "JAVA_HOME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|home| PathBuf::from(home).join("lib").join("modules"))
        .find(|p| p.is_file())
}

fn strict_boot_registry() -> NativeMethodRegistry {
    let mut registry = NativeMethodRegistry::new();
    // Before registration, as `vm_init` does: refused rows never land.
    registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
    boot_path::vm_init_real_jdk_boot_path(&mut registry);
    registry
}

fn kind_name(kind: NativeKind) -> &'static str {
    match kind {
        NativeKind::Bridge => "bridge",
        NativeKind::Intrinsic => "intrinsic",
        _ => "other",
    }
}

fn opcode_name(opcode: u8) -> &'static str {
    match opcode {
        OP_INVOKEVIRTUAL => "virtual",
        OP_INVOKESPECIAL => "special",
        OP_INVOKESTATIC => "static",
        _ => "interface",
    }
}

#[test]
#[ignore = "reads a JDK 25 image; set CRATONVM_JDK25_HOME and run with --ignored"]
fn thin_wrapper_bridge_census() {
    let Some(modules) = jdk_image() else {
        println!(
            "thin-wrapper census: no <home>/lib/modules under CRATONVM_JDK25_HOME, \
             CRATONVM_JAVA_HOME or JAVA_HOME; nothing measured"
        );
        return;
    };
    let mut image = Image::open(&modules)
        .unwrap_or_else(|| panic!("{} is not a readable jimage", modules.display()));

    let strict = strict_boot_registry();
    // Last registration wins, as dispatch does.
    let mut live: BTreeMap<(String, String, String), NativeKind> = BTreeMap::new();
    for (c, m, d, k) in strict.dump_registrations() {
        live.insert((c.to_string(), m.to_string(), d.to_string()), k);
    }
    let bridges: Vec<(String, String, String)> = live
        .iter()
        .filter(|(_, k)| **k == NativeKind::Bridge)
        .map(|(t, _)| t.clone())
        .collect();
    assert!(
        bridges.len() >= 2000,
        "only {} live Bridge rows in the strict boot registry -- the replay broke",
        bridges.len()
    );

    let mut tiers: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut not_in_image = 0usize;
    let mut same_fn_rows = 0usize;
    for (c, m, d) in &bridges {
        let Some(decl) = image.declaration(c, m, d) else {
            not_in_image += 1;
            continue;
        };
        if decl.is_native || decl.is_abstract {
            continue;
        }
        let Some((call, (to_c, to_m, to_d))) = decl.thin else {
            continue;
        };
        let target_key = (to_c.clone(), to_m.clone(), to_d.clone());
        let target_native = live.get(&target_key).copied();
        let target_decl = image.declaration(&to_c, &to_m, &to_d);
        let target_image = match &target_decl {
            Some(t) if t.is_native => "acc-native",
            Some(t) if t.is_abstract => "abstract",
            Some(_) => "java",
            None => "unresolved",
        };
        let live_native = matches!(
            target_native,
            Some(NativeKind::Bridge) | Some(NativeKind::Intrinsic)
        );
        let tier = if live_native && decl.is_acc_bridge {
            "A-acc-bridge"
        } else if live_native && target_image == "acc-native" {
            "B-acc-native"
        } else if live_native {
            "C-live-native"
        } else {
            "D-java"
        };
        *tiers.entry(tier).or_default() += 1;
        // The wrapper row and its target bound to the SAME function is the
        // mechanical form of "the answer cannot change". `no` is not a
        // disproof: two identical closures are two functions.
        let same_fn = match (strict.find(c, m, d), strict.find(&to_c, &to_m, &to_d)) {
            (Some(a), Some(b)) => std::ptr::fn_addr_eq(a, b),
            _ => false,
        };
        if same_fn {
            same_fn_rows += 1;
        }
        println!(
            "thin-wrapper\t{tier}\t{c}.{m}{d}\tdeclared-on={}\t{}\t{to_c}.{to_m}{to_d}\t{}\t\
             target-native={}\ttarget-image={target_image}\tsame-fn={}",
            decl.on,
            opcode_name(call.opcode),
            if call.forwards_exactly {
                "exact"
            } else {
                "partial"
            },
            target_native.map(kind_name).unwrap_or("-"),
            if same_fn { "yes" } else { "no" },
        );
    }
    let total: usize = tiers.values().sum();
    println!(
        "thin-wrapper census: {} live Bridge rows, {not_in_image} not in the image, \
         {total} thin wrappers ({same_fn_rows} bound to their target's function): {tiers:?}",
        bridges.len()
    );
    // Round 15 read of the 25/linux kind map: 257 thin wrappers over `bridge`
    // rows, before this wave retired eleven. A number near zero means the
    // matcher or the image read broke, not that the population went away.
    assert!(
        total >= 50,
        "only {total} thin wrappers found; the matcher or the image read broke"
    );
}

/// The matcher on hand-read JDK 25 bodies (`javap -c`), so the census cannot
/// pass by finding nothing. Five positives, the shapes CA6-1's first step
/// named, and the negatives that keep it a ONE-call matcher.
#[test]
fn the_matcher_reads_hand_read_bodies() {
    // System.setOut(PrintStream): aload_0; invokestatic setOut0; return
    let t = match_thin_wrapper(
        &[0x2a, 0xb8, 0x00, 0x07, 0xb1],
        true,
        "(Ljava/io/PrintStream;)V",
    )
    .expect("System.setOut is thin");
    assert_eq!(
        (t.opcode, t.cp_index, t.forwards_exactly),
        (OP_INVOKESTATIC, 7, true)
    );

    // Array.newInstance(Class, int): aload_0; iload_1; invokestatic newArray; areturn
    let t = match_thin_wrapper(
        &[0x2a, 0x1b, 0xb8, 0x00, 0x09, 0xb0],
        true,
        "(Ljava/lang/Class;I)Ljava/lang/Object;",
    )
    .expect("Array.newInstance is thin");
    assert!(t.forwards_exactly);

    // ServerSocketChannel.bind(SocketAddress): aload_0; aload_1; iconst_0;
    // invokevirtual bind(SocketAddress, int); areturn -- thin, not exact.
    let t = match_thin_wrapper(
        &[0x2a, 0x2b, 0x03, 0xb6, 0x00, 0x05, 0xb0],
        false,
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
    )
    .expect("ServerSocketChannel.bind(SocketAddress) is thin");
    assert_eq!((t.opcode, t.forwards_exactly), (OP_INVOKEVIRTUAL, false));

    // TypeVariableImpl.toString(): aload_0; invokevirtual getName; areturn
    let t = match_thin_wrapper(
        &[0x2a, 0xb6, 0x00, 0x02, 0xb0],
        false,
        "()Ljava/lang/String;",
    )
    .expect("TypeVariableImpl.toString is thin");
    assert!(t.forwards_exactly);

    // A javac generic bridge: aload_0; aload_1; checkcast; invokevirtual
    // compareTo(T); ireturn.
    let t = match_thin_wrapper(
        &[0x2a, 0x2b, 0xc0, 0x00, 0x03, 0xb6, 0x00, 0x04, 0xac],
        false,
        "(Ljava/lang/Object;)I",
    )
    .expect("a generic bridge is thin");
    assert!(t.forwards_exactly);

    // Two-slot parameters: static (JI)J forwarded as lload_0; iload_2.
    let t = match_thin_wrapper(&[0x1e, 0x1c, 0xb8, 0x00, 0x01, 0xad], true, "(JI)J")
        .expect("wide slots");
    assert!(t.forwards_exactly);
    // The same loads in the wrong order are thin but not a forward.
    let t = match_thin_wrapper(&[0x1b, 0x1a, 0xb8, 0x00, 0x01, 0xac], true, "(II)I")
        .expect("swapped loads");
    assert!(!t.forwards_exactly);

    // ParameterizedTypeImpl.getRawType()Class: aload_0; getfield; areturn.
    assert!(match_thin_wrapper(
        &[0x2a, 0xb4, 0x00, 0x02, 0xb0],
        false,
        "()Ljava/lang/Class;"
    )
    .is_none());
    // Two calls.
    assert!(match_thin_wrapper(
        &[0x2a, 0xb6, 0x00, 0x01, 0x57, 0x2a, 0xb6, 0x00, 0x02, 0xb0],
        false,
        "()Ljava/lang/Object;"
    )
    .is_none());
    // An `ldc` argument.
    assert!(match_thin_wrapper(
        &[0x12, 0x03, 0xb8, 0x00, 0x04, 0xb0],
        true,
        "()Ljava/lang/Object;"
    )
    .is_none());
    // Trailing bytes after the return.
    assert!(match_thin_wrapper(
        &[0x2a, 0xb6, 0x00, 0x02, 0xb0, 0x00],
        false,
        "()Ljava/lang/String;"
    )
    .is_none());
    // A leading checkcast has nothing to cast.
    assert!(match_thin_wrapper(&[0xc0, 0x00, 0x01, 0xb8, 0x00, 0x02, 0xb1], true, "()V").is_none());
}

#[test]
fn arg_slots_counts_two_slot_types_and_arrays() {
    assert_eq!(arg_slots("()V", true), Some(vec![]));
    assert_eq!(arg_slots("()V", false), Some(vec![0]));
    assert_eq!(arg_slots("(JI)J", true), Some(vec![0, 2]));
    assert_eq!(
        arg_slots("([JLjava/lang/String;D[[Ljava/lang/Object;)V", false),
        Some(vec![0, 1, 2, 3, 5])
    );
    assert_eq!(arg_slots("V", true), None);
}
