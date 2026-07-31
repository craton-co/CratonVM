# The forced-native `java/lang/String` policy exists twice, in two opposite forms, and the two disagree — one block is statically unreachable

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31. **DANGEROUS:
the disagreement is silent, and it has already made a landed, measured
performance fix into dead code.**

## What is wrong

CratonVM forces 21 concrete `java/lang/String` bytecode methods to lose to
registered natives. That policy is written down **twice**, in two files, in two
opposite representations, with in-code comments on both sides asking that they
be kept in sync by hand:

| Path | File | Form |
|---|---|---|
| Cold / vtable-miss (`check_override`) | `vm/src/vm/vm_exec.rs` | **positive** list — 21 method *names*, no descriptors |
| Warm / per-call-site cached (`force_native_over_real_jdk_bytecode`) | `vm/src/runtime/interpreter/invoke.rs` | **negative** list — `return false` for `String` unless the (name, descriptor) pair is one of 7 |

This is the single largest deliberate violation of contract §1.4 ("concrete Java
bytecode wins over any registered native, except for a reviewed
`NativeKind::Intrinsic`") in the tree.

## Evidence

### The positive list — `vm/src/vm/vm_exec.rs`, in `check_override` (~line 18467)

```rust
|| (class_name == "java/lang/String"
    && matches!(
        method_name,
        "charAt" | "length" | "isEmpty" | "equals" | "hashCode"
        | "indexOf" | "lastIndexOf" | "substring" | "startsWith"
        | "endsWith" | "trim" | "toString" | "concat" | "replace"
        | "toLowerCase" | "toUpperCase" | "compareTo"
        | "compareToIgnoreCase" | "equalsIgnoreCase" | "contains" | "split"
    ))
```

21 names, matched by **name only** — every overload of every listed method.
Its own preceding comment states the reason and the intended lifetime:

> RKC16N.6 RECON (Session 94): real-JDK `java/lang/String` bytecode resolution
> is failing for these basic methods during JDK class clinits like
> `java/nio/charset/StandardCharsets.<clinit>`; route to our layout-neutral
> natives (registered in `register_essential_natives`) so the boot can advance
> past `String` dispatch. **Drop when RKC16N.6 lands a permanent fix.**

The neighbouring `StringUTF16.getChars` entry carries the hand-sync request
outright: *"Keep this concrete bytecode override in sync with `interpreter.rs`'s
`force_native_over_real_jdk_bytecode` gate."*

### The negative list — `vm/src/runtime/interpreter/invoke.rs`, in `force_native_over_real_jdk_bytecode` (~line 7280)

```rust
if class_name == "java/lang/String"
    && !matches!(
        (method_name, method_descriptor),
        ("replaceAll",   "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;")
      | ("replaceFirst", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;")
      | ("matches",      "(Ljava/lang/String;)Z")
      | ("replace",      "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;")
      | ("substring",    "(II)Ljava/lang/String;")
      | ("<init>",       "([BLjava/lang/String;)V")
      | ("<init>",       "([BIILjava/lang/String;)V")
    )
{
    return false;
}
```

Seven exact (name, descriptor) pairs survive. Everything else on `String` is
handed back to real bytecode **at this path**, while the same method is still
forced native at the other path.

### The consequence nobody intended: a landed perf fix is unreachable

`force_native_over_real_jdk_bytecode` is a single function spanning roughly
lines 6946–9268 of `invoke.rs`, evaluated as a linear sequence of
`if … { return … }` blocks. The `return false` above sits at ~7280. Two later
blocks in the same function try to force `String` methods native:

* ~8684 — `("substring", "(II)…")` → **reachable** (it is one of the seven).
* ~8715 — `("substring", "(I)…")`, `("charAt", "(I)C")`, `("length", "()I")`,
  `("isEmpty", "()Z")`, `("startsWith", "(Ljava/lang/String;)Z")` → **none of
  these five is in the seven-pair whitelist, so control never reaches this
  block.**

The ~8715 block is not incidental code. Its own comment describes it as a fix
root-caused against an H2 BNF-autocomplete workload:

> PERF (h2-bnf-perf 2026-07-23): same "gate mismatch" family as the `(II)`
> substring entry immediately above — `check_override` (`vm_exec.rs`) has listed
> `charAt`/`length`/`isEmpty`/`startsWith` … as forced-native since "RKC16N.6
> RECON", but that allowlist is only consulted on a genuine vtable cache miss —
> this function (the per-call-site cached vtable fast path) never had the
> matching entries, so once a call site's cache warmed, real (fully interpreted)
> bytecode ran regardless of `check_override`'s intent, for the entire remaining
> lifetime of that call site.

That comment diagnoses the gate mismatch correctly and then adds the entries
*below* the very `return false` that causes it. The two halves of the same
policy defeated each other, in the same function, and nothing reported it.

**Not established:** whether the ~7280 exclusion was added before or after the
~8715 block. Both blame to the same file-split commit (`0e233796ab`), so
ordinary `git blame` cannot answer it. Establishing it needs
`git log -S` archaeology on the distinctive literals, or — better — a runtime
probe (see *How to verify*). Do not assume the h2-bnf fix ever worked.

## Why it was not fixed in wave 1

Contract §7 puts both lists in scope for wave 2 explicitly: *"The hard-coded
class-name exception lists (`ThreadPoolExecutor.execute` receiver-shape special
case, the forced-native `String` method list) are wave-2 removals — leave them
in place but funnel them through `resolve_dispatch`."* Deleting either half
alone desynchronises the cold and warm paths further; deleting both requires the
underlying RKC16N.6 real-JDK `String` bytecode-resolution failure to be fixed
first, which is a boot-critical change wave 1 could not validate.

## What specifically must change

1. **Fix RKC16N.6** — real-JDK `java/lang/String` bytecode resolution during JDK
   `<clinit>`s. That is the actual defect; both lists are workarounds for it.
2. Delete both `String` arms together. They cannot be removed independently:
   the positive list governs the cold path, the negative one the warm path, and
   removing either alone changes behaviour only for call sites in one temperature
   regime — which is exactly the kind of divergence that produced the dead block
   above.
3. Separate the *performance* entries from the *correctness workaround*
   entries before deleting anything. `replaceAll`/`replaceFirst`/`matches`/
   `replace(CharSequence,CharSequence)` are a deliberate, flagged fast-regex
   optimisation (`CRATONVM_NATIVE_STRING_REGEX`, default-ON), not an RKC16N.6
   workaround; under contract §1.4 they belong as reviewed
   `NativeKind::Intrinsic` registrations, which win with no name list at all.
4. While both lists still exist, make the disagreement impossible to reintroduce:
   a single shared `const` table consulted by both sites, or a unit test that
   asserts the two predicates agree for every (name, descriptor) either one
   mentions.

## How to verify a fix

* **Unreachability, statically:** a unit test calling
  `force_native_over_real_jdk_bytecode("java/lang/String", "charAt", "(I)C")`
  must return `true` once the gate mismatch is fixed. It returns `false` today.
* **Unreachability, at runtime:** the h2-bnf autocomplete workload
  (`org.h2.bnf.RuleFixed` / `RuleElement` / `Bnf`) is the workload the ~8715
  block was written for; its `s = s.substring(1)` / `s.charAt(0)` /
  `s.length()` / `up.startsWith(name)` inner loop is the measurement. If
  removing the ~7280 exclusion changes that workload's time, the block was dead.
* **Parity:** the two paths must produce the same dispatch verdict for the same
  triple. A cheap check is a debug assertion at the warm path comparing its
  verdict to `check_override`'s for `java/lang/String`.
* After the lists are gone: the `--jdk-only` census must report zero
  `native-shadows-bytecode` violations for `java/lang/String`.

## Blast radius if done wrong

* Deleting the positive list before RKC16N.6 is fixed regresses **boot**:
  `java/nio/charset/StandardCharsets.<clinit>` is on the critical path.
* Deleting the negative list alone forces `String` native on every warm call
  site, including `trim`/`toLowerCase`/`toUpperCase`/`compareTo*` — which the
  h2-bnf comment deliberately excluded because they have "Unicode/locale edge
  cases that need their own from-scratch correctness review". Forcing those
  natives is a *correctness* change, not a performance one.
* Fixing the reachability bug without reviewing the five newly-live methods
  silently changes `charAt`/`length`/`isEmpty`/`startsWith`/`substring(I)`
  semantics on every warm call site in the VM at once.

## Related

* [`NativeKind` is ambient and defaults to `SyntheticStub`](native-kind-is-ambient-and-defaults-to-syntheticstub.md)
  — the reason these natives cannot simply be re-tagged `Intrinsic` today.
* [Cached invoke targets drop the `NativeKind`](cached-invoke-targets-drop-the-nativekind.md)
  — the structural reason a cold-path policy and a warm-path policy exist at all.
