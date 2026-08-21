# H21-1 — `Pipe.open()` returned an instance of an abstract class, in both modes

**Status: FIXED IN SOURCE, NOT YET VERIFIED BY AN ARM.** Lane H21, 2026-08-21.

**Provenance.** Lane H21 completed its source edits and died to an
infrastructure fault while narrating a change it had **already written**. Its
last words were *"Now register `source`/`sink` on `PipeImpl`"* — and that
registration is present in the diff. **Reconstructed by lane H0 from the lane's
own in-source comments**; every `MEASURED` line is quoted from a comment written
beside the code it describes. **Not re-measured by H0.**

---

## 1. The defect, stated plainly

```java
Pipe.open().getClass()
```

| | class | `Modifier.isAbstract` |
|---|---|---|
| HotSpot 25.0.3+9 | `sun.nio.ch.PipeImpl` | false |
| CratonVM **both modes** | **`java.nio.channels.Pipe`** | **true** |

**The VM was handing out an object whose runtime class is abstract.** JVMS §6.5
makes that an `InstantiationError` for `new` — it is a receiver the bytecode
`new` opcode cannot legally produce, and the VM produced it anyway.

The mechanism is one line: `ensure_class_initialized("java/nio/channels/Pipe")`
resolves to the **real, abstract** JDK class, and `alloc_object` then mints an
object of it.

**Only the wrapper was wrong.** The two channels were already correct
(`SourceChannelImpl` / `SinkChannelImpl`). So the object graph was right
everywhere except its root.

## 2. Why this is the P1 row's real defect

`H5-1` and `H11-2` together disproved the published P1 remedy — *"move the
natives from the abstract public API onto the `sun.nio.ch.*Impl` classes"* —
by measuring that **237 of 334 `native-io` rows cannot move**, because the VM
mints receivers whose class name IS the abstract class. `H11-2` also showed
`Pipe` **already registers on both**, which is why the row's own example was the
one that made least sense.

This record is the other half: the registration was never the problem.
**The VM instantiating an abstract class was.** Fix the mint and the
registration question becomes ordinary.

## 3. Why the field slots survive the change — measured, not hoped

```
$ javap -p sun.nio.ch.PipeImpl        # JDK 25.0.3+9
  private final sun.nio.ch.SourceChannelImpl source;   // slot 0
  private final sun.nio.ch.SinkChannelImpl   sink;     // slot 1
```

The same two slots, in the same order, that `PIPE_WRAPPER_FIELD_SOURCE` /
`_SINK` already name, holding values of exactly the declared types. So the
writes now land on the **real fields** instead of on private slots above a
zero-field layout — and `PipeImpl.source()`'s own bytecode **would return the
right object even if it ran**.

It does not run: `source`/`sink` are registered on `sun/nio/ch/PipeImpl` as
well as on the abstract class, and **dispatch keys on the receiver** (`H11-1`,
measured), so the natives keep answering. Without the `PipeImpl` row the real
bytecode would take over — which would also work, but silently, and that is not
a change to make by accident.

`Pipe.open()` itself is **static** — no receiver — so its registration stays on
`java/nio/channels/Pipe`, where the constant-pool class is the key. The two
halves of `H11-1`'s dispatch answer are both load-bearing here, in opposite
directions, at one call site.

## 4. The fallback, and what it still does

```rust
let pipe_cid = match ctx.ensure_class_initialized("sun/nio/ch/PipeImpl") {
    Ok(cid) => cid,
    Err(_) => ctx.ensure_class_initialized("java/nio/channels/Pipe")
                 .unwrap_or_else(|_| ClassId::new(0)),
};
```

`PipeImpl` when the image has it; the abstract class only when it does not,
which is the synthetic-JDK shape. **The abstract mint therefore still exists on
that path** — this fixes the real-JDK arm and leaves the no-class-library arm
as it was. Stated because a reader grepping for
`ensure_class_initialized("java/nio/channels/Pipe")` will still find it and
should not conclude the fix did not land.

The lane wrote it as a `match` rather than `.or_else(|_| ctx…)` deliberately:
the closure form needs a second mutable borrow of `ctx`, and **a lane forbidden
to build must not lean on a borrow-checker judgement it cannot check.** That is
the right instinct and worth copying.

## 5. NOT VERIFIED

* **No build and no arm** has run against this change.
* The `ClassId::new(0)` in the final fallback is the **untyped-allocation
  sentinel** — the one `H0-6` measured as substituting
  `cratonvm/synthetic/AnonymousObject$N`. On that path the wrapper becomes a
  fabricated 2-field carrier. It is a pre-existing shape, not introduced here,
  but it means the fallback-of-the-fallback is itself a known defect class.
* `socket_channel.rs` was changed in the same patch and is **not described
  here**, because the lane died before documenting it. Its diff should be read
  before this page is cited as complete.
* Whether any vector actually exercises `Pipe.open()`'s class identity is
  **unknown**. If none does, this fix is unfalsifiable by the corpus.

## 6. NOMINATIONS

* **N1 — a `getClass()` row for every VM-minted receiver.** This defect was
  invisible for the same reason `H19-1`'s was: the values were right and only
  the class was wrong. `H18`'s `OpcodeVsReflectionProbe` is the natural home.
* **N2 — grep every `ensure_class_initialized` whose argument is an abstract or
  interface type.** `H11-2` found 14 classes' worth of minted abstract
  receivers; this fixes one. The others are the same one-line shape.
* **N3 — `Modifier.isAbstract(o.getClass().getModifiers())` is a one-line
  universal assertion** and no vector makes it. Any VM-minted receiver failing
  it is a defect by JVMS §6.5, with no oracle run required.

---

## VERIFIED ON A BUILD (lane H0, 2026-08-21)

Built at `757a9cf4f` (`cratonvm-r8.exe`, 0 errors), `--jdk-only`:

```
                        CratonVM                      HotSpot 25.0.3+9
Pipe.open()   ->  sun.nio.ch.PipeImpl  abstract=false   sun.nio.ch.PipeImpl  abstract=false
source        ->  sun.nio.ch.SourceChannelImpl          sun.nio.ch.SourceChannelImpl
sink          ->  sun.nio.ch.SinkChannelImpl            sun.nio.ch.SinkChannelImpl
```

**Byte-identical to the oracle on all three lines**, against
`java.nio.channels.Pipe` / `abstract=true` before. The VM no longer hands out an
instance of an abstract class here.

§5's caveat that "whether any vector exercises `Pipe.open()`'s class identity is
unknown" is now moot for the fix itself — this probe exercises it directly — but
remains true for the corpus, which still asks nothing about it. The
`Modifier.isAbstract` assertion nominated in §6 N3 would have caught this defect
with no oracle run at all, and is still not in the suite.
