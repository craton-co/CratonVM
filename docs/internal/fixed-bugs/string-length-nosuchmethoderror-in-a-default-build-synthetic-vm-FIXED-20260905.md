# `"abcdef".length()` raised `NoSuchMethodError` in a default-build embedded VM — FIXED 2026-09-05

**Status:** FIXED, verified by `cargo test` on Azure host 2 (see §6).
**Files:** `vm/src/vm/vm_init.rs`, `native-builtins/src/service_loader.rs`.
**Census:** `docs/known-issues/jdk-only/F30-1-the-registrar-call-graph-and-the-drifted-arm-20260813.md`
§9 carries the registrar-graph half of this record and is where the gate that
protects the fix is described.

---

## 1. The symptom

In a **default** `cargo` build (no `synthetic-jdk` feature), the documented
embedding path

```rust
let mut vm = Vm::new(VmConfig::new().with_classpath(cp));
vm.invoke("cratonvm/SlProbe2", "literalLength", "()I", &[]);
```

against

```java
public static int literalLength() {
    String s = "abcdef";
    return s.length();   // <-- java/lang/NoSuchMethodError
}
```

raised `java/lang/NoSuchMethodError`. So did `String.isEmpty()`,
`String.charAt(int)`, `String.equals(Object)`, and
`ServiceLoader.load(Class)`. `String.hashCode()` worked — by falling through to
`java/lang/Object`, which is the detail that made the shape legible.

## 2. Why it was not noticed for a long time

The ~5,000-test in-tree suite runs in exactly this configuration and mostly
does not touch the affected surface. Two tests did:

* `vm/tests/wp7_2_jdbc_core_types_reachable.rs::connection_methods_carry_signatures`
  — its fixture is the only in-tree caller that reaches `String.length()` on a
  `Method.getName()` result. It returned its `catch (Throwable)` sentinel
  `-100`. Its passing twin, `result_set_next_reflects_with_boolean_return`,
  differs by using `"next".equals(n)` instead — which fell through to
  `Object.equals` and got the right answer by interning.
* `vm/tests/wp1_8_real_jar_serviceloader.rs::driver_discovered_from_jar_on_classpath`
  — returned `-99`.

Both were red on `dev`. Both are **green under `--features synthetic-jdk`**,
before and after this change, which is what identified the defect as
configuration-specific rather than as a JDBC, jar, or class-loading defect.
Neither test names `String` or the JDK mode, so neither read as one.

## 3. The mechanism

Three facts, each individually reasonable:

1. **`VmConfig::default()` is `JdkMode::Synthetic`.**
   `EMBEDDED_DEFAULT_JDK_MODE` (`vm/src/config.rs`) selects it deliberately, to
   keep the library path hermetic. `SharedVm::new` therefore skips
   boot-classpath discovery, and `java/lang/String` is a VM-minted carrier
   declaring five methods with no `Code` attribute anywhere:

   ```text
   CLS java/lang/String src=None nmeth=5 super=Some(ClassId(1))
   CLS java/lang/String methods=["join(…)", "join(…)", "replace(CC)…",
                                 "toLowerCase(Locale)…", "toUpperCase(Locale)…"]
   ```

2. **The synthetic class library is not compiled into a default build.**
   `register_builtins` and `register_synthetic_overrides` — the ~5,200 stubs
   that *are* that library — are `#[cfg(feature = "synthetic-jdk")]`;
   `vm/src/native/builtins.rs` supplies no-op stubs when the feature is off.

3. **Arm B dropped the essential bridges anyway.** The
   `#[cfg(not(feature = "synthetic-jdk"))]` block in `vm_init.rs` opened with
   an unconditional `native_methods.set_drop_real_layout_synthetic(true)`, and
   `NativeMethodRegistry::register` drops every `java/lang/String` `Bridge`
   when that flag is set. `register_essential_natives_with_shims` registers
   `String.length()`, `isEmpty()`, `equals()` and the rest a few lines later,
   and they were dropped on the way in.

No bytecode (1), no synthetic override (2), and the one remaining
implementation dropped (3).

Fact 3 is the correctable one — you cannot runtime-gate code that is not
compiled — and it is also the one answering the wrong question. The flag's own
rationale is *"a fake 5-field layout corrupts the real 7-field object"*, which
presupposes a real object. In a synthetic run there is none to protect and the
drop is pure loss. The Cargo feature decides what is **compiled**; the JDK mode
decides which **class library loads**. Those are different questions and the
`#[cfg]` was answering the first one.

`NativeMethodRegistry::drops_real_layout_synthetic()`'s doc comment already
said this, in as many words, before the defect was found:

> A `#[cfg(feature = "synthetic-jdk")]` guard is NOT equivalent and must not be
> used for this: the Cargo feature decides what is COMPILED, the launcher flag
> decides which CLASS LIBRARY loads.

## 4. The fix

```rust
// vm/src/vm/vm_init.rs, the #[cfg(not(feature = "synthetic-jdk"))] arm
if !config.use_synthetic_jdk {
    native_methods.set_drop_real_layout_synthetic(true);
}
```

This is the same correction `register_synthetic_aqs_natives` and
`register_cyclic_barrier_natives` already carry in the sibling arm, whose
comments say it in the same words ("Runtime-gated on `use_synthetic_jdk`, not
on the Cargo feature"). No registration pass became conditional.

`service_loader::register_service_loader_natives` had the identical hole with a
different mechanism — its body was `#[cfg(feature = "synthetic-jdk")]`, so the
2026-08-29 retirement of the nine `java/util/ServiceLoader` stubs in favour of
the JDK's own bytecode also removed them in a run that has no such bytecode.
Its gate now reads `r.drops_real_layout_synthetic()`, which `vm_init` sets
before every caller of that registrar.

## 5. What the fix does NOT change

* Real-JDK runs are byte-for-byte unaffected: the flag is still set, before the
  same passes, in every configuration that has real JDK bytecode.
* No `register_*` call moved, was added, or became conditional, so F30's
  arm-parity census and
  `the_two_real_jdk_arms_run_the_same_registrars_in_the_same_order` are
  untouched.
* The 2026-08-29 ServiceLoader retirement stands where it was argued: a
  real-JDK run still gets zero `java/util/ServiceLoader` registrations and
  still runs the lazy JDK iterator.

## 6. Verification

`cargo test --workspace --no-fail-fast` on Azure host 2, default features.

Directly relevant targets, all green after the change:

| target | before | after |
|---|---|---|
| `wp7_2_jdbc_core_types_reachable` | 9 passed, 1 **failed** | 10 passed |
| `wp1_8_real_jar_serviceloader` | 1 passed, 1 **failed** | 2 passed |
| `wp7_1_jdbc_driver_loader` | 4 passed | 4 passed |
| `wp8_10_9_string_contains_native` | 6 passed | 6 passed (now in real-JDK mode) |
| `cratonvm-vm --lib` `registrar_call_graph_witness` | 4 passed, 1 **failed** by design | 5 passed |

Three tests were asking about a mode they were not in, and now say which mode
they mean rather than inheriting it from the build:

* `wp8_10_9_string_contains_native.rs` booted `VmConfig::default()` and called
  the result "a real-JDK registry" — its own header had already flagged this
  class of mistake one layer up. It now boots `JdkMode::Real`, and skips
  loudly when no JDK image is reachable rather than asserting nothing in
  silence.
* `wp7_1_jdbc_driver_loader.rs::jdbc_driver_natives_export_service_loader` and
  `native-builtins/src/jdbc.rs`'s two `service_loader_*` unit tests used bare
  registries with one `#[cfg]` polarity each. They are now a pair compiled in
  both builds, each setting the flag it is asking about.

The F30 gate was updated rather than deleted: it now requires **exactly one**
`use_synthetic_jdk` read in arm B and pins, line for line, that the one read is
the layout-drop decision. A second branch — a genuine fourth registration path
— still goes red.

## 7. The general lesson

A guard keyed on a Cargo feature is a claim about the build. A guard keyed on
`config.use_synthetic_jdk` is a claim about the run. Wherever the fallback for
"drop this native" is *"the real bytecode will answer it"*, the question is
about the run, because the run is what decides whether that bytecode is loaded.
Three separate registrars have now been found with this exact confusion
(`register_synthetic_aqs_natives`, `register_cyclic_barrier_natives`, and now
the layout drop plus `register_service_loader_natives`), each surfacing as a
`NoSuchMethodError` or `UnsatisfiedLinkError` on a class that "obviously"
works. F30 §8 already nominates the systematic sweep; this is a fourth data
point for it.
