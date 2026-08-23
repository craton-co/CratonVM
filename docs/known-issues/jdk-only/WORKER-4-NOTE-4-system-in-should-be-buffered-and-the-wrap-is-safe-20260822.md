# WORKER-4-NOTE-4 — `System.in` should be a `BufferedInputStream`, the wrap is MEASURED SAFE, and the one-file patch is written out here

**Status: MEASURED. No source change — the fix belongs in `vm/`, which this lane
does not own, so it is written out rather than made** (the convention `H11-3` N1
used and this brief §5.3 asks for). 2026-08-22, Linux (Azure host 2), Temurin
25.0.4+7.

## 1. The divergence, and the part of it that is not cosmetic

`WORKER-4-1` N4 recorded that `System.in` is a `java.io.FileInputStream` where
HotSpot installs a `java.io.BufferedInputStream`. MEASURED,
`regression-suite/probes/W4Stdin.java`, both modes:

```text
  System.in.getClass()          HotSpot BufferedInputStream   CratonVM FileInputStream
  System.in instanceof BIS      HotSpot true                  CratonVM false
  System.in.markSupported()     HotSpot TRUE                  CratonVM FALSE
```

**The third is a capability, not an identity.**
`if (System.in.markSupported()) { in.mark(n); … in.reset(); }` is how a parser
decides whether it may look ahead and rewind, and on this VM it takes the
no-lookahead branch on a stream HotSpot lets it rewind.

## 2. The refusal is overturned: wrapping is MEASURED SAFE

N4 declined the fix on the grounds that the stdin carrier is a
`FileInputStream`-SHAPED object whose fd lives in a SLOT rather than behind a
working `read`, so wrapping it might yield a `System.in` that reads nothing.

**That was reasoned, not measured — and no probe in this lane had ever fed
stdin.** Every earlier run saw an empty stream, where *"reads nothing"* and
*"works correctly"* are the same observation. `W4Stdin` is run with input piped
in, one mode per process (stdin is consumed by whichever reader goes first, so
they cannot share a run). With `alpha beta\nsecond line\nthird\n` on stdin, ALL
FIVE consumption paths are byte-identical to the oracle in both modes:

```text
  System.in.read() / read(byte[],int,int) / readAllBytes()      identical
  new BufferedInputStream(System.in) — the same three           identical
  new Scanner(System.in)                                        identical
  new Scanner(new BufferedInputStream(System.in))               identical   <- the exact fear
  new BufferedReader(new InputStreamReader(System.in))          identical
```

The fourth line settles it. A `Scanner` over an ALREADY-WRAPPED `System.in` was
the shape the refusal was about, and it works — because
`native_scanner_init_inputstream`'s general drain (2026-08-22, `WORKER-4-2` §5)
reads any stream through its own virtual `read(byte[],int,int)` instead of
duck-typing its slots. **The change that unblocked this was made earlier in the
same lane, and nothing connected the two until the probe was written.**

## 3. Where the fix does NOT go, measured

The obvious site is `native-builtins/src/lang_system.rs::native_system_init_phase1`,
which builds the carrier and does `set_static_field_by_name("java/lang/System",
"in", …)`. This lane wrote the wrap there, built it, and **nothing changed**.

`--dump-native-registry`, both modes:

```text
  java/lang/System.initPhase1()V    owns_slot=True   invocations=0   kind=bridge
```

**The native never runs.** Its own registration comment says as much — it is a
"no-op fallback" for a boot path that does not occur here. An edit there is dead
code, and the wrap it installs would be overwritten regardless (§4).

Recorded because "put it where the object is built" is the natural guess and it
is wrong twice over.

## 4. Where it DOES go: `vm/src/vm/vm_util.rs`

`System.in` is installed by the VM itself, on `java/lang/System`'s class
initialisation, in the `GETSTATIC` bridge that also installs `out` and `err`:

```rust
if class_name.as_deref() == Some("java/lang/System") {
    let (out_ref, err_ref) = shared.ensure_system_streams();
    let in_ref = ensure_system_stdin_object(shared, thread)?;
    …
    if let Some(in_idx) = field_indices.2 {
        super::set_static_shared(shared, class_id, in_idx, Value::Object(Some(in_ref)));
    }
}
```

That is the only writer that survives, which is why the `lang_system.rs` edit
was inert even before being dead.

**The patch, for whoever owns `vm/`.** Wrap at the install point and keep the
CARRIER for everything that asks for it by slot — `ensure_system_stdin_object`'s
own callers, and `Scanner`'s fd fast path, want the object whose slot holds the
descriptor, not a wrapper around it. Only the STATIC FIELD changes:

```rust
let in_ref = ensure_system_stdin_object(shared, thread)?;
// `System.in` is a BufferedInputStream on every real JVM, and the difference
// is reachable: `System.in.markSupported()` answers false without it, so a
// parser that asks before looking ahead takes the wrong branch.
// WORKER-4-NOTE-4 measures all five consumption paths through the wrap as
// byte-identical to HotSpot, including `new Scanner(new
// BufferedInputStream(System.in))`.
// The CARRIER stays the cached stdin object; only the static field is wrapped.
// A failure to build the wrapper installs the carrier exactly as before —
// this must not become a new way to come up without a `System.in`.
let in_static = wrap_stdin_buffered(shared, thread, in_ref).unwrap_or(in_ref);
…
super::set_static_shared(shared, class_id, in_idx, Value::Object(Some(in_static)));
```

where `wrap_stdin_buffered` allocates `java/io/BufferedInputStream` and invokes
its `(Ljava/io/InputStream;)V` constructor with `in_ref`, returning `None` on any
failure. The same two-step this lane used for
`java.io.UTFDataFormatException` and `java.io.UnsupportedEncodingException`, and
that `afc_closed_channel_error` has used all along.

**What to verify:** `regression-suite/probes/W4Stdin.java` is in the tree and is
the gate. The target is the four identity/capability lines matching and all five
consumption modes staying byte-identical. It needs stdin fed — a run without it
proves nothing, which is the trap this note exists to close.

## 5. What is NOT claimed

* **Whether anything in the corpus depends on the current shape.** No scheduled
  vector reads stdin; that is why this survived. A lane making the change should
  expect the suite to be silent about it either way, and rely on the probe.
* **The `--jdk-only` half.** Strict mode has the same carrier and the same
  `markSupported() == false`; this is not a compatible-mode-only defect, and the
  patch is not mode-conditional.
