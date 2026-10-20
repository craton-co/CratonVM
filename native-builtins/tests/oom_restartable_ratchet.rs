// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! OOM-RESTARTABLE RATCHET (ZGC round zs, wave s1, lane nd).
//!
//! `src/oom_restartable.rs` names the native BODIES the VM's native funnel may
//! collect and RE-RUN after one of their one-attempt allocation doors refused
//! (`vm_exec::native_oom_replay`, `CRATONVM_ZGC_NATIVE_OOM_REPLAY`, ZGC only).
//! Re-running a native is correct only when running it from the start twice is
//! the same as running it once: nothing Java-visible may change before its
//! last allocation (the admission rule in that module's doc). The admission is
//! a review, and before this gate nothing re-checked it: a later edit that made
//! a listed body mutate an argument before an allocation would make the replay
//! apply the mutation twice, silently, and only on a near-full heap
//! (`docs/internal/gc/zgc-zr11nd-native-oom-replay-residuals-20261013.md`
//! §3).
//!
//! Two exact gates, zero slack, in the house ratchet style
//! (`stub_ratchet.rs`, `r13_misc10_create_string_ratchet.rs`):
//!
//! 1. [`the_restartable_list_is_the_audited_list`]: the list in
//!    `restartable_bodies()` equals [`AUDITED`], in order. Adding a body
//!    without adding its audited row here fails; so does removing one (drop
//!    its row).
//! 2. [`each_listed_body_keeps_its_audited_effect_census`]: for every listed
//!    body, the number of Java-visible-effect calls IN ITS OWN TEXT (field and
//!    array writes, Java invocations, class definition, monitors, static
//!    writes) equals the frozen count. The audit established that each counted
//!    write targets an object the body allocated itself (or happens after the
//!    body's last allocation); a new one has to be audited the same way, and
//!    the gate makes that a deliberate edit of this table instead of an
//!    accident.
//!
//! When it fails:
//! * a NEW listed body: audit it against the rule (cite the code that proves
//!   every effect is on a fresh object or after the last allocation, and that
//!   paired state such as a thread-local depth is closed on the unwind), then
//!   add its row with the census numbers the failure prints;
//! * a CHANGED census on a listed body: re-audit that body. If the new effect
//!   is on an object the body allocated itself (or after its last allocation),
//!   update the row and say why in the commit; if it touches an argument, an
//!   existing object or runs Java, the body is no longer restartable: take it
//!   off the list.
//!
//! Blind spots, stated: the census reads the body's own text only, not the
//! helpers it calls (an effect moved into a helper is invisible here), and it
//! cannot tell a fresh object from an argument (that is what the audit row
//! records). Comments and string literals are skipped.

use std::path::Path;

/// One audited restartable body.
struct Audited {
    /// The path exactly as `restartable_bodies()` spells it.
    path: &'static str,
    /// The crate source file that defines it (relative to `src/`).
    file: &'static str,
    /// Frozen census of the body's own text: field / array WRITES.
    writes: usize,
    /// Frozen census: Java invocations, class definition, monitor ops, static
    /// writes. Zero for every body audited so far.
    java_or_global: usize,
    /// Where the audit is recorded.
    audit: &'static str,
}

const Z11ND: &str = "docs/internal/zgc-round-zr-20261012/z11-nd-report.md §1-2";
const ZS1ND: &str = "docs/internal/zgc-round-zs-20261014/s1-nd-report.md";
const GCI_OE47: &str = "gci i47w lane oe47 (9b881973be): Integer/Long.valueOf restartable";
const GCI_I31OM: &str = "gci i31/om (94235b1d65): the Class.getName leaf body; zt t1/nd (row restored after the zs/dev merge)";

/// The audited list, in `restartable_bodies()` order.
const AUDITED: &[Audited] = &[
    Audited {
        path: "crate::lang_class::native_class_get_declared_fields",
        file: "lang_class.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::lang_class::native_class_get_declared_methods",
        file: "lang_class.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::lang_class::native_class_get_declared_constructors",
        file: "lang_class.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::lang_class::native_class_get_methods",
        file: "lang_class.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::lang_class::native_class_get_interfaces",
        file: "lang_class.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::lang_class::native_class_get_declared_classes",
        file: "lang_class.rs",
        // The one write fills the body's own fresh `Class[]` (`arr`, minted by
        // `new_ref_array` and pinned before the mirror loop).
        writes: 1,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::lang_class::native_class_get_name",
        file: "lang_class.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        // The `CRATONVM_CLASS_GET_NAME_MEMO` leaf callback: a memo read, a
        // leaf decline, then `native_class_get_name_memoizing` (whose one
        // unwinding allocation is `native_class_get_name`'s intern, made
        // before the memo row's refusing `try_new_ref_array` and its root).
        // zt t1/nd: dev listed it in `restartable_bodies()` (gci i31/om) and
        // the zs/dev merge kept the entry but not this row, so the gate failed.
        path: "crate::lang_class::native_class_get_name_leaf",
        file: "lang_class.rs",
        writes: 0,
        java_or_global: 0,
        audit: GCI_I31OM,
    },
    Audited {
        path: "crate::lang_system::native_thread_get_stack_trace",
        file: "lang_system.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::native_thread_dump_threads",
        file: "lib.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::native_thread_get_threads",
        file: "lib.rs",
        // The one write fills the body's own fresh `Thread[]` (`arr`, from the
        // reclaiming `try_new_ref_array` ladder, inside a `NativeHandleScope`).
        writes: 1,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::native_integer_to_string_compat",
        file: "lib.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::native_long_to_string_compat",
        file: "lib.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::native_double_to_string_compat",
        file: "lib.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::native_integer_to_hex_string_compat",
        file: "lib.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::native_object_get_class",
        file: "lib.rs",
        writes: 0,
        java_or_global: 0,
        audit: Z11ND,
    },
    Audited {
        path: "crate::lang_string::native_sb_get_value",
        file: "lang_string.rs",
        // Reads the receiver (`sb_view`, `sb_units_range_view`), one fresh
        // `byte[]`, then `sb_put_units_at` into THAT array. zs s2/rv: the
        // helper is a needle now, so its one write is counted (it was 0: the
        // census was blind to the body's only write).
        writes: 1,
        java_or_global: 0,
        audit: ZS1ND,
    },
    Audited {
        path: "crate::lang_math::native_integer_value_of",
        file: "lang_math.rs",
        // One fresh `Integer` (`alloc_wrapper`), its `value` written, then the
        // cache slot published by CAS after the body's last allocation (the
        // second counted write: the VM's own cache table, not a Java object).
        writes: 2,
        java_or_global: 0,
        audit: GCI_OE47,
    },
    Audited {
        path: "crate::lang_math::native_long_value_of",
        file: "lang_math.rs",
        // The `Long` twin of the row above.
        writes: 2,
        java_or_global: 0,
        audit: GCI_OE47,
    },
    Audited {
        path: "crate::intrinsics::callback_for(cratonvm_native_api::InterpIntrinsic::IntegerValueOf)",
        file: "",
        // The cached interpreted callback for `Integer.valueOf`: it resolves to
        // the `native_integer_value_of` body audited above, so it has no text
        // of its own to census.
        writes: 0,
        java_or_global: 0,
        audit: GCI_OE47,
    },
    Audited {
        path: "crate::intrinsics::callback_for(cratonvm_native_api::InterpIntrinsic::LongValueOf)",
        file: "",
        writes: 0,
        java_or_global: 0,
        audit: GCI_OE47,
    },
];

/// Method-call needles that WRITE a Java-visible field or array element.
const WRITE_NEEDLES: &[&str] = &[
    ".set_field(",
    ".set_field_raw(",
    ".set_field_volatile(",
    ".set_field_by_name(",
    ".set_fields_by_name(",
    ".set_fields_typed(",
    ".set_array_element(",
    ".write_byte_array_from(",
    ".write_int_array_from(",
    ".write_char_array_from(",
    ".write_primitive_array_bytes(",
    ".bulk_array_copy(",
    ".copy_reference_array_elements(",
    // zs s2/rv: effect methods of `NativeContext` the first list missed, and
    // the StringBuilder store helper (a whole body's writes went through it).
    ".compare_and_swap_field(",
    ".atomic_fetch_add_",
    ".copy_to_native_memory(",
    "sb_put_units_at(",
];

/// Needles that run Java, define a class, take a monitor or write a static.
const JAVA_OR_GLOBAL_NEEDLES: &[&str] = &[
    ".invoke(",
    ".invoke_by_class_id(",
    ".invoke_static",
    ".invoke_virtual",
    ".invoke_special",
    ".invoke_method",
    ".call_method",
    ".define_class",
    ".define_hidden_class",
    ".redefine_class",
    ".monitor_enter",
    ".monitor_exit",
    ".monitor_wait",
    ".monitor_notify",
    ".set_static_field",
    ".new_object_initialized",
    // zs s2/rv: process / thread / class state the first list missed.
    ".set_current_thread_object(",
    ".set_native_thread_java_obj(",
    ".set_system_property(",
    ".set_class_hidden(",
];

/// `text` with comments and the contents of string / char literals blanked
/// (replaced by spaces, newlines kept), so needles and braces inside them are
/// not counted. Raw strings (`r"..."`, `r#"..."#`) and byte strings are
/// handled; a `'` that does not close a char literal is a lifetime.
fn strip_comments_and_literals(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let blank = |out: &mut Vec<u8>, c: u8| out.push(if c == b'\n' { b'\n' } else { b' ' });
    while i < b.len() {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                blank(&mut out, b[i]);
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0usize;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    out.extend_from_slice(b"  ");
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    out.extend_from_slice(b"  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    blank(&mut out, b[i]);
                    i += 1;
                }
            }
        } else if c == b'r'
            && (b.get(i + 1) == Some(&b'"') || b.get(i + 1) == Some(&b'#'))
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
        {
            // Raw string: r"..." or r#..#"..."#..#
            let mut j = i + 1;
            let mut hashes = 0usize;
            while b.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) != Some(&b'"') {
                out.push(c);
                i += 1;
                continue;
            }
            out.push(b'r');
            for _ in i + 1..=j {
                out.push(b' ');
            }
            i = j + 1;
            loop {
                if i >= b.len() {
                    break;
                }
                if b[i] == b'"' && (0..hashes).all(|k| b.get(i + 1 + k) == Some(&b'#')) {
                    for _ in 0..=hashes {
                        out.push(b' ');
                    }
                    i += 1 + hashes;
                    break;
                }
                blank(&mut out, b[i]);
                i += 1;
            }
        } else if c == b'"' {
            out.push(b' ');
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    blank(&mut out, b[i]);
                    if i + 1 < b.len() {
                        blank(&mut out, b[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                if b[i] == b'"' {
                    out.push(b' ');
                    i += 1;
                    break;
                }
                blank(&mut out, b[i]);
                i += 1;
            }
        } else if c == b'\'' {
            // A char literal is 'x', '\n', '\u{..}', '\''; anything else is a
            // lifetime or label.
            let close = if b.get(i + 1) == Some(&b'\\') {
                (i + 2..b.len().min(i + 12)).find(|&k| b[k] == b'\'')
            } else if b.get(i + 2) == Some(&b'\'') {
                Some(i + 2)
            } else {
                // Multi-byte UTF-8 scalar: up to 4 bytes then a quote.
                (i + 2..b.len().min(i + 6)).find(|&k| b[k] == b'\'' && b[i + 1] >= 0x80)
            };
            match close {
                Some(end) => {
                    for _ in i..=end {
                        out.push(b' ');
                    }
                    i = end + 1;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The body (from its opening `{` to its matching `}`) of the free function
/// `name` in the already-stripped `text`, or `None`.
fn fn_body<'a>(stripped: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("fn {name}(");
    let mut from = 0;
    while let Some(rel) = stripped[from..].find(&needle) {
        let at = from + rel;
        from = at + needle.len();
        // `fn` must start a token.
        if at > 0 {
            let prev = stripped.as_bytes()[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        let bytes = stripped.as_bytes();
        let open = at + stripped[at..].find('{')?;
        // A `;` before the `{` outside any bracket is a declaration without a
        // body (the `;` of an array type `[T; N]` in the signature is not).
        let mut nest = 0i32;
        let declaration = stripped[at..open].bytes().any(|ch| {
            match ch {
                b'(' | b'[' => nest += 1,
                b')' | b']' => nest -= 1,
                _ => {}
            }
            ch == b';' && nest <= 0
        });
        if declaration {
            continue;
        }
        let mut depth = 0usize;
        for (k, &ch) in bytes.iter().enumerate().skip(open) {
            match ch {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&stripped[open..=k]);
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    None
}

fn count_needles(body: &str, needles: &[&str]) -> usize {
    needles.iter().map(|n| body.matches(n).count()).sum()
}

fn src_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The `crate::...` paths of the array literal in `restartable_bodies()`, and
/// the array length its signature declares.
fn listed_bodies() -> (Vec<String>, usize) {
    let path = src_dir().join("oom_restartable.rs");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let stripped = strip_comments_and_literals(&text);
    let sig_at = stripped.find("fn restartable_bodies()").expect(
        "oom_restartable.rs no longer defines `fn restartable_bodies()`: the gate is blind",
    );
    let sig = &stripped[sig_at..];
    let len_at = sig
        .find("; ")
        .expect("`[NativeCallback; N]` return type not found")
        + 2;
    let len_end = len_at + sig[len_at..].find(']').expect("array length not closed");
    let declared: usize = sig[len_at..len_end]
        .trim()
        .parse()
        .expect("array length is not a literal");
    let body = fn_body(&stripped, "restartable_bodies").expect("restartable_bodies has no body");
    let paths = body
        .split(|c: char| {
            c == ',' || c == '[' || c == ']' || c.is_whitespace() || c == '{' || c == '}'
        })
        .filter(|tok| tok.starts_with("crate::"))
        .map(str::to_string)
        .collect();
    (paths, declared)
}

#[test]
fn the_restartable_list_is_the_audited_list() {
    let (listed, declared) = listed_bodies();
    assert_eq!(
        listed.len(),
        declared,
        "restartable_bodies() declares {declared} entries but lists {}",
        listed.len()
    );
    let audited: Vec<&str> = AUDITED.iter().map(|a| a.path).collect();
    let listed_ref: Vec<&str> = listed.iter().map(String::as_str).collect();
    let added: Vec<&&str> = listed_ref.iter().filter(|p| !audited.contains(p)).collect();
    let removed: Vec<&&str> = audited.iter().filter(|p| !listed_ref.contains(p)).collect();
    assert!(
        added.is_empty() && removed.is_empty() && listed_ref == audited,
        "the OOM-restartable list changed without its audit.\n  \
         listed but not audited (audit it against the rule in oom_restartable.rs, then add \
         a row to AUDITED in this file): {added:?}\n  \
         audited but no longer listed (drop its row): {removed:?}\n  \
         listed order: {listed_ref:?}"
    );
    for a in AUDITED {
        assert!(
            !a.audit.is_empty(),
            "{}: an audited row must cite its audit",
            a.path
        );
    }
}

#[test]
fn each_listed_body_keeps_its_audited_effect_census() {
    let mut failures = Vec::new();
    for a in AUDITED {
        if a.file.is_empty() {
            // An intrinsic callback (`callback_for(..)`): its body is a row of
            // its own above.
            continue;
        }
        let name = a.path.rsplit("::").next().unwrap();
        let file = src_dir().join(a.file);
        let text =
            std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let stripped = strip_comments_and_literals(&text);
        let Some(body) = fn_body(&stripped, name) else {
            failures.push(format!(
                "{}: `fn {name}(` not found in src/{} (moved? update the row's file)",
                a.path, a.file
            ));
            continue;
        };
        let writes = count_needles(body, WRITE_NEEDLES);
        let java = count_needles(body, JAVA_OR_GLOBAL_NEEDLES);
        if writes != a.writes || java != a.java_or_global {
            failures.push(format!(
                "{}: effect census writes={writes} java_or_global={java}, audited writes={} \
                 java_or_global={} ({}). Re-audit the body: an effect on an argument or an \
                 existing object before the last allocation makes it NOT restartable.",
                a.path, a.writes, a.java_or_global, a.audit
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_census_reads_code_not_comments_or_literals() {
    let src = r##"
fn listed(ctx: &mut dyn C, args: &[Value]) -> R {
    // ctx.set_field(a, 0, v) in a comment
    /* ctx.set_array_element(a, 0, v) /* nested */ still a comment */
    let s = "ctx.set_field(x) and { braces }";
    let r = r#"ctx.invoke_static("{")"#;
    let c = '{';
    let l: &'static str = "x";
    ctx.set_field(fresh, 0, Value::Int(1));
    ctx.invoke_static("A", "b", "()V", &[]);
    if true { ctx.set_array_element(arr, 0, Value::Int(2)); }
}
fn other() { ctx.set_field(a, 0, v); }
"##;
    let stripped = strip_comments_and_literals(src);
    let body = fn_body(&stripped, "listed").expect("body");
    assert_eq!(count_needles(body, WRITE_NEEDLES), 2);
    assert_eq!(count_needles(body, JAVA_OR_GLOBAL_NEEDLES), 1);
    assert!(!body.contains("fn other"));
    assert_eq!(fn_body(&stripped, "missing"), None);
    // A token that merely ends in the name is not the function.
    let s2 = strip_comments_and_literals("fn not_listed(a: u8) { x.set_field(); }");
    assert_eq!(fn_body(&s2, "listed"), None);
}
