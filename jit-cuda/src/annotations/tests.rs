use super::*;
use cratonvm_reader::attribute::ElementValuePair;

/// The Rust match arms and the Java enums must name the same
/// constants.
///
/// # Why this test is the load-bearing one for this file
///
/// Every annotation value this crate understands is matched BY
/// STRING against a `craton.gpu.*` enum that lives in a DIFFERENT
/// repository (`craton-gpu-java`), located at build time by
/// `$CRATON_GPU_JAVA_SRC`, a sibling checkout, or — on Windows — an
/// absolute path. Nothing links the two versions. Renaming a Java
/// constant, or adding one, breaks nothing that either side can see:
/// javac still compiles, cargo still builds, and every existing test
/// still passes.
///
/// What changes is behaviour, silently. `parse_admission_hint` falls
/// back to `Strict`, which merely loses coverage. `parse_grid_shape`
/// used to fall back to `Elementwise`, which does not: it would turn
/// `grid = BLOCK_REDUCTION` into an element-wise kernel and produce
/// wrong answers — the exact failure AUDIT 2026-08-28 fixed by
/// rejecting unimplemented shapes, re-opened through the name
/// channel. That fallback is now `GridShape::Unknown`, which the
/// analyzer rejects; this test is the other half, catching the drift
/// at build time rather than discovering it as a wrong answer.
///
/// # Skipping, loudly
///
/// The annotation classes are only present when the build found the
/// external project. When they are absent this test reports a skip
/// and passes, because failing would make the whole crate untestable
/// on a machine that has no reason to check out a second repository.
/// It prints what it did either way, so a CI run that covered
/// nothing says so instead of showing a green tick.
#[test]
fn rust_enum_names_match_the_java_definitions() {
    let dir = env!("CRATON_GPU_CLASSES_DIR");
    if dir.is_empty() {
        eprintln!(
            "SKIP rust_enum_names_match_the_java_definitions: the craton-gpu-java \
             project was not found at build time, so there are no compiled \
             annotation classes to check against. Set CRATON_GPU_JAVA_SRC to \
             enable this check."
        );
        return;
    }
    let mut checked = 0usize;
    for (class, rust_names) in [
        ("GridShape", &super::GRID_SHAPE_NAMES[..]),
        ("AdmissionHint", &super::ADMISSION_HINT_NAMES[..]),
    ] {
        let path = std::path::Path::new(dir)
            .join("craton")
            .join("gpu")
            .join(format!("{class}.class"));
        if !path.is_file() {
            eprintln!(
                "SKIP {class}: {} is not present, though the classes \
                 directory is",
                path.display()
            );
            continue;
        }
        let bytes =
            std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let class_file = cratonvm_reader::class_reader::read_class(&bytes)
            .unwrap_or_else(|e| panic!("parse {}: {e:?}", path.display()));
        // An enum's constants are its `static final` fields typed as
        // the enum itself. `$VALUES` is the synthetic array javac
        // adds and is typed as an array, so the descriptor match
        // excludes it without needing to know its name.
        let want_descriptor = format!("Lcraton/gpu/{class};");
        let mut java_names: Vec<String> = class_file
            .fields
            .iter()
            .filter(|f| {
                f.is_static() && f.is_final() && &*f.descriptor == want_descriptor.as_str()
            })
            .map(|f| f.name.to_string())
            .collect();
        java_names.sort();
        let mut rust_sorted: Vec<String> = rust_names.iter().map(|s| s.to_string()).collect();
        rust_sorted.sort();
        assert_eq!(
            java_names, rust_sorted,
            "craton.gpu.{class} and this crate's parser disagree about the \
             constant set.\n  Java: {java_names:?}\n  Rust: {rust_sorted:?}\n\
             A name only Java has is a value this crate will not recognise; \
             a name only Rust has is a match arm that can never fire. Update \
             the parser in annotations.rs and the list beside it.",
        );
        checked += 1;
    }
    eprintln!("rust_enum_names_match_the_java_definitions: checked {checked} enum(s)");
}

/// Tiny builder that hands out fresh constant-pool indices and
/// produces a `ConstantPool` at the end. Tests use this instead of
/// hand-counting slots.
struct CpBuilder {
    entries: Vec<ConstantPoolEntry>,
}

impl CpBuilder {
    fn new() -> Self {
        Self {
            entries: vec![ConstantPoolEntry::Tombstone],
        }
    }
    fn utf8(&mut self, s: &str) -> u16 {
        let idx = self.entries.len() as u16;
        self.entries.push(ConstantPoolEntry::Utf8(s.into()));
        idx
    }
    fn integer(&mut self, v: i32) -> u16 {
        let idx = self.entries.len() as u16;
        self.entries.push(ConstantPoolEntry::Integer(v));
        idx
    }
    fn build(self) -> ConstantPool {
        ConstantPool::new(self.entries)
    }
}

/// Build a `RuntimeInvisibleAnnotations` attribute from one or more
/// pre-built `Annotation`s.
fn ria(annotations: Vec<Annotation>) -> Attribute {
    Attribute::RuntimeInvisibleAnnotations(annotations)
}

#[test]
fn parse_gpu_kernel_default_admit() {
    // `@GpuKernel` with no element pairs — every field must be the
    // §2.1 default.
    let mut cp = CpBuilder::new();
    let desc = cp.utf8(GPU_KERNEL_DESC);
    let cp = cp.build();
    let ann = Annotation {
        type_index: desc,
        element_value_pairs: vec![],
    };
    let m = read_method_annotations(&[ria(vec![ann])], &cp);
    let k = m.gpu_kernel.expect("@GpuKernel must be detected");
    assert_eq!(k, GpuKernelAttrs::default());
    assert_eq!(k.grid, GridShape::Elementwise);
    assert_eq!(k.admit, AdmissionFlags::STRICT);
    // Spec §2.1: 0 means "JVM picks the block dimension".
    assert_eq!(k.block_x, 0);
    assert_eq!(k.block_y, 0);
    assert_eq!(k.block_z, 0);
    assert_eq!(k.shared_bytes, 0);
    assert!(m.gpu_exclude.is_none());
}

#[test]
fn parse_gpu_kernel_with_allow_allocation() {
    // `@GpuKernel(admit = ALLOW_ALLOCATION)`.
    let mut cp = CpBuilder::new();
    let desc = cp.utf8(GPU_KERNEL_DESC);
    let admit_name = cp.utf8("admit");
    let admit_type = cp.utf8("Lcraton/gpu/AdmissionHint;");
    let admit_const = cp.utf8("ALLOW_ALLOCATION");
    let cp = cp.build();
    let ann = Annotation {
        type_index: desc,
        element_value_pairs: vec![ElementValuePair {
            element_name_index: admit_name,
            value: ElementValue::Enum {
                type_name_index: admit_type,
                const_name_index: admit_const,
            },
        }],
    };
    let m = read_method_annotations(&[ria(vec![ann])], &cp);
    let k = m.gpu_kernel.expect("@GpuKernel must be detected");
    assert_eq!(k.admit, AdmissionHint::AllowAllocation.into());
    // Untouched fields still take their defaults (spec §2.1: 0).
    assert_eq!(k.grid, GridShape::Elementwise);
    assert_eq!(k.block_x, 0);
}

#[test]
fn parse_gpu_exclude() {
    // `@GpuExclude(reason = "test")`.
    let mut cp = CpBuilder::new();
    let desc = cp.utf8(GPU_EXCLUDE_DESC);
    let reason_name = cp.utf8("reason");
    let reason_val = cp.utf8("test");
    let cp = cp.build();
    let ann = Annotation {
        type_index: desc,
        element_value_pairs: vec![ElementValuePair {
            element_name_index: reason_name,
            value: ElementValue::Const {
                tag: b's',
                const_value_index: reason_val,
            },
        }],
    };
    let m = read_method_annotations(&[ria(vec![ann])], &cp);
    let e = m.gpu_exclude.expect("@GpuExclude must be detected");
    assert_eq!(e.reason, "test");
    assert!(m.gpu_kernel.is_none());
}

#[test]
fn parse_enable_gpu_async_warmup_3() {
    // `@EnableGpuAsync(warmup = 3)` on a class.
    let mut cp = CpBuilder::new();
    let desc = cp.utf8(ENABLE_GPU_ASYNC_DESC);
    let warmup_name = cp.utf8("warmup");
    let warmup_val = cp.integer(3);
    let cp = cp.build();
    let ann = Annotation {
        type_index: desc,
        element_value_pairs: vec![ElementValuePair {
            element_name_index: warmup_name,
            value: ElementValue::Const {
                tag: b'I',
                const_value_index: warmup_val,
            },
        }],
    };
    let c = read_class_annotations(&[ria(vec![ann])], &cp);
    let a = c.enable_async.expect("@EnableGpuAsync must be detected");
    assert_eq!(a.warmup, 3);
}

#[test]
fn unknown_annotation_ignored() {
    // `@Lcom/example/Other;` is not one of our three — the reader
    // returns the all-default `MethodAnnotations` (no `gpu_kernel`,
    // no `gpu_exclude`).
    let mut cp = CpBuilder::new();
    let desc = cp.utf8("Lcom/example/Other;");
    let cp = cp.build();
    let ann = Annotation {
        type_index: desc,
        element_value_pairs: vec![],
    };
    let m = read_method_annotations(&[ria(vec![ann.clone()])], &cp);
    assert_eq!(m, MethodAnnotations::default());
    assert!(m.gpu_kernel.is_none());
    assert!(m.gpu_exclude.is_none());

    let c = read_class_annotations(&[ria(vec![ann])], &cp);
    assert_eq!(c, ClassAnnotations::default());
    assert!(c.enable_async.is_none());
}
/// The array spelling composes; the single-constant one still means
/// what it always meant.
///
/// AUDIT 2026-09-02. `admit` was one-of, so a kernel could ask for
/// `Math.sqrt` OR an unguarded division and never both — even though
/// the two decisions have nothing to do with each other. This is the
/// request that was previously unsayable.
#[test]
fn admit_accepts_a_set_of_constants() {
    let mut cp = CpBuilder::new();
    let hint_type = cp.utf8("Lcraton/gpu/AdmissionHint;");
    let intrinsics = cp.utf8("ALLOW_INTRINSIC_CALLS");
    let div = cp.utf8("ALLOW_DIV_BY_ZERO");
    let value = ElementValue::Array(vec![
        ElementValue::Enum {
            type_name_index: hint_type,
            const_name_index: intrinsics,
        },
        ElementValue::Enum {
            type_name_index: hint_type,
            const_name_index: div,
        },
    ]);

    let flags = parse_admission_flags(&value, &cp.build());

    assert!(flags.intrinsic_calls, "the first constant was dropped");
    assert!(
        flags.unguarded_integer_division,
        "the second constant was dropped"
    );
    assert!(
        flags.approximate_float_remainder,
        "ALLOW_DIV_BY_ZERO has admitted frem/drem since 2026-07-11 and \
         must keep doing so, in either spelling"
    );
    assert!(!flags.allocation, "nothing asked for allocation");
}

/// The two decisions `ALLOW_DIV_BY_ZERO` used to carry together can
/// now be told apart.
///
/// It still SETS both — that constant is what users have written and
/// its meaning does not change. What changed is that the analyzer
/// reads two independent bits, so a future constant can loosen one
/// without the other, and the reason `Reason::FloatRemainder` gives
/// for the coupling ("would require editing `annotations.rs`") is no
/// longer a reason for anything.
#[test]
fn the_two_division_decisions_are_separate_bits() {
    let both: AdmissionFlags = AdmissionHint::AllowDivByZero.into();
    assert!(both.unguarded_integer_division);
    assert!(both.approximate_float_remainder);

    // Independently settable, which the enum could not express.
    let only_integer = AdmissionFlags {
        unguarded_integer_division: true,
        ..AdmissionFlags::STRICT
    };
    assert!(!only_integer.approximate_float_remainder);
    let only_float = AdmissionFlags {
        approximate_float_remainder: true,
        ..AdmissionFlags::STRICT
    };
    assert!(!only_float.unguarded_integer_division);    }

/// Every legacy hint converts to exactly the set it always meant.
#[test]
fn every_hint_converts_to_the_set_it_always_meant() {
    assert_eq!(
        AdmissionFlags::from(AdmissionHint::Strict),
        AdmissionFlags::STRICT
    );
    assert_eq!(
        AdmissionFlags::from(AdmissionHint::AllowAllocation),
        AdmissionFlags {
            allocation: true,
            ..AdmissionFlags::STRICT
        }
    );
    assert_eq!(
        AdmissionFlags::from(AdmissionHint::AllowIntrinsicCalls),
        AdmissionFlags {
            intrinsic_calls: true,
            ..AdmissionFlags::STRICT
        }
    );
}

/// An unrecognised constant loosens nothing.
///
/// The safe direction, and the opposite of `parse_grid_shape`'s:
/// every bit here LOOSENS admission, so a name this crate does not
/// know leaves the analyzer stricter than asked and can never admit
/// something the emitter cannot lower.
#[test]
fn an_unknown_constant_loosens_nothing() {
    let mut cp = CpBuilder::new();
    let hint_type = cp.utf8("Lcraton/gpu/AdmissionHint;");
    let bogus = cp.utf8("ALLOW_EVERYTHING_PLEASE");
    let value = ElementValue::Enum {
        type_name_index: hint_type,
        const_name_index: bogus,
    };
    assert_eq!(
        parse_admission_flags(&value, &cp.build()),
        AdmissionFlags::STRICT
    );
}
