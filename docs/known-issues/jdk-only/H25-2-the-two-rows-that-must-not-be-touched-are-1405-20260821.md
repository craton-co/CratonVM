# H25-2 — `H14-1`'s "2 rows must not be touched" is 2 in the census and **1,405** in the registry, and `java/nio/file/Path` alone holds 45 of them

**Status: OPEN — MEASURED.** One `--dump-native-registry --explain-jdk-only
--jdk-only` dump from the prebuilt `C:/craton/cratonvm-r8.exe` (`025780ff7`),
plus `javap -p` against the oracle image (Microsoft JDK 25.0.3.9-hotspot) for
all 193 receiver classes involved, 0 failures. **No source change; no build.**
Lane H25, 2026-08-21. Companion to `H25-1` (same two dumps).

`H14-1` §4 is the record every retirement plan now quotes:

> | declared, no `Code`, not `ACC_NATIVE` (abstract/interface) | **2** | 0.1% |
>
> **The 2 are `java/nio/file/Path.toString()` and `Path.equals(Object)`** …
> **A retirement of those two removes the only implementation there is.**

That is correct and it is a floor. Measured against the **registry** rather than
the **census**, the same shape is **1,405 registrations over 193 classes** —
and 21 more of them are on `java/nio/file/Path` itself.

---

## 1. The measurement

Partition all 10,378 `--jdk-only` registrations on `image_declaring_method`:

| image verdict for the registered triple | registrations | share |
|---|---:|---:|
| declared on the named class, **has `Code`** — `H14-1`'s *retire* shape | 4,802 | 46.3% |
| inherited from a supertype — `H14-1`'s *relocate* shape | 2,056 | 19.8% |
| **declared, NO `Code`, not `ACC_NATIVE`** — abstract or interface | **1,405** | **13.5%** |
| receiver class absent from the image | 999 | 9.6% |
| declared `ACC_NATIVE` — a legitimate bridge under §1.5 | 774 | 7.5% |
| method declared nowhere (`H25-1`) | 342 | 3.3% |
| **total** | **10,378** | 100% |

* **1,280 of the 1,405 own their slot** — they are the reachable registration.
* **193 distinct receiver classes**, and `javap -p` on every one of them splits
  cleanly with no residue:

| receiver kind | classes | registrations |
|---|---:|---:|
| `interface` | 134 | **923** |
| `abstract class` | 59 | **482** |
| concrete class | **0** | **0** |

Zero concrete receivers is the sanity check that the partition means what it
says: a no-`Code` method on a concrete class would have been a measurement bug.

### 1.1 Where they are

| registrations | source file |
|---:|---|
| 234 | `native-collections/src/lib.rs` |
| 115 | `native-builtins/src/jmx.rs` |
| 108 | `native-builtins/src/phases_late/jdbc.rs` |
| 102 | `native-builtins/src/phases_late/nio_file.rs` |
| 92 | `native-builtins/src/servlet.rs` |
| 91 | `native-builtins/src/net_phase_e.rs` |
| 73 | `native-io/src/lib.rs` |
| 71 | `native-builtins/src/phases_late/foreign_ffm.rs` |
| 70 | `native-builtins/src/panama.rs` |
| 62 | `native-builtins/src/xml_stax.rs` |
| 61 | `native-builtins/src/phases_late/ssl_security.rs` |

By class: `java/lang/foreign/MemorySegment` 87, **`java/nio/file/Path` 45**,
`javax/xml/stream/XMLStreamReader` 43, `java/nio/ByteBuffer` 38,
`java/util/stream/{Int,Long,Double}Stream` 36/35/34, `java/util/stream/Stream`
32, `javax/management/MBeanServer` 30, `javax/net/ssl/SSLSession` 27,
`java/lang/management/ThreadMXBean` 25, `java/nio/channels/DatagramChannel` 24.

### 1.2 `java/nio/file/Path` in full — 45 registrations, 21 owning a slot

`H14-1` found 2 because the census records **dispatched** shadows and the corpus
dispatched 2. The registry holds:

```
compareTo ×3  endsWith ×2  equals ×2  getFileName ×2  getFileSystem
getName ×3  getNameCount ×3  getParent ×2  getRoot ×2  hashCode ×2
isAbsolute ×3  normalize ×3  register  relativize  resolve ×2
startsWith ×2  subpath ×2  toAbsolutePath ×3  toRealPath ×2
toString ×2  toUri ×2
```

**24 distinct methods, 21 of them owning a slot.** `toString` and `equals` — the
two `H14-1` names — are two of twenty-one. Every one of the other nineteen is
the same shape, on the same interface, from the same registrar family, and no
record in this directory names any of them.

## 2. What the number does and does not mean

**This is the correction that matters, and it cuts against the headline.**

`H14-1`'s 2 are *proven fatal*: the corpus dispatched them, `Path` is an
interface, and retirement removes the only implementation of a call that
actually happens. The 1,405 are **not** 1,405 proven-fatal rows. A no-`Code`
method on an interface is normally served by a **concrete implementor** — a
`Stream.map` call has a `ReferencePipeline` receiver whose `map` has plenty of
bytecode, so retiring a native registered on `java/util/stream/Stream` may well
be perfectly safe.

The honest claim is narrower and still large:

> **1,405 registrations are ones for which the standard retirement argument —
> "real JDK bytecode exists behind it, so deleting the native leaves something
> to run" — is FALSE AS STATED.** Whether anything runs depends on the concrete
> class of the actual receiver, and **the registry dump cannot answer that.**

So the 1,405 is a **triage population, not a hazard count**: `H14-1` §4's
1244-row *retire* bucket is the population where the argument holds without
further work, and this is the population where every row needs a
receiver-level answer first.

Why `Path` is the bad case and `Stream` probably is not: CratonVM's `Path`
receivers are the interface carrier itself — that is `H5-1` §3's
abstract-receiver mechanism, and `abstract recv` ("no `Code` attribute means
abstract receiver, not bad dispatch") is the standing note for reading it. The
question for each of the 1,405 is *"does this VM ever hold a receiver whose
concrete class supplies the method?"*, and it is answered per class, not per
row.

## 3. Three consequences for work already planned

1. **Arming a prefix arms the landmines too.** `H14-3` row 11 armed
   `register_phase57_nio_file` — `Files`, `Path`, `Paths`,
   `FileSystemProvider`, `BufferedWriter`, `Arrays$ArrayList` — and measured
   **98/104, five real failures** (`RCrypto`, `RFileTimes`, `RNioNoFollow`,
   `RForeignLayoutJdkInterfaces`, `RJdkNio`). That arm suppressed **102
   no-`Code` registrations** from `nio_file.rs`, 45 of them on `Path`. The cell
   is a correct measurement of the dial; it is **not** a price for retiring that
   registrar's *retirable* rows, because the dial also removed twenty-one
   implementations that have no bytecode behind them. `H14-3` §6 already says
   the dial is not the retirement — this quantifies by how much for that row.

   **`H17-2` (landed while this record was being written) sharpens the same
   point from the other side:** the dial is wired to **one** dispatch door, so
   arming a class only affects calls that reach step 1 cold. An arm is therefore
   BOTH over-broad (it suppresses the 21 `Path` implementations a source
   retirement would keep) and under-broad (it misses the other doors a source
   retirement would close). It is not a bound in either direction.
2. **`H14-1` §4's three verbs need a fourth and a fifth.** *Retire* (1244),
   *relocate* (156), *do not touch* (2) — plus `H25-1`'s **dead / near-miss**
   (342) and this record's **needs a receiver answer** (1,405). A plan with one
   verb does the wrong thing 11% of the time (`H14-2` §7.3); a plan with three
   verbs still has no verb for 1,747 registrations.
3. **The *relocate* prescription is wrong for at least one row, and probably a
   family.** `H14-1` says of the 156 inherited rows: *"Retirement is the wrong
   verb here — the registration is on the wrong class."* For
   `java/lang/Package.equals(Ljava/lang/Object;)Z`
   (`lang_class.rs:19516`, `owns_slot: true`, inherited from `java/lang/Object`)
   that prescription would **break a documented fix**. The registrar's own
   comment says why: HotSpot interns one `Package` per (loader, name) so
   identity `equals` suffices; CratonVM allocates a **fresh** synthetic
   `Package` on every `Class.getPackage()`, so identity equality is always
   false, and Spring's `MvcParamPredicate.hasMvcAnnotation` silently
   misclassified annotations by declaring package until this override was added.
   It is a **deliberate override of an inherited method**, not a misplaced
   registration. Nobody has checked how many of the other 155 are that shape.

## 4. What this does NOT establish

* **It does not establish that 1,405 rows are unretirable** (§2). It establishes
  that the argument used to justify retiring them does not apply, and that
  nobody has done the receiver analysis. Reading this record as "1,405
  landmines" would be the mirror of the error it corrects.
* **No row here was retired, armed, probed or otherwise tested.** This is a
  static image adjudication plus `javap`. The only dynamic evidence in the
  neighbourhood is `H14-3` row 11, and that is quoted, not re-run.
* **One image, one platform.** `H25-1` §1.6 applies here too: this is Microsoft
  JDK 25.0.3.9 on windows/x64. A method that is abstract on this image is
  overwhelmingly likely to be abstract on all of them — `Path` has been an
  interface since JDK 7 — so the exposure is far smaller than for `H25-1`'s 342,
  but it is not zero and no multi-image sweep was run.
* **`javap`'s receiver-kind split is by the first declaration line**, a text
  heuristic. It produced 0 residue and 0 concrete classes across 193 classes,
  which is why it is quoted, but it is a heuristic.
* **The 156-row *relocate* bucket was NOT re-audited.** §3.3 names exactly one
  row where the prescription fails and argues the family is worth checking. The
  count of how many is **unmeasured**.

## 5. NOMINATIONS

* **N1 — answer the receiver question per CLASS, not per row.** 193 classes, and
  the top ten hold 45% of the registrations. For each: does this VM ever hold a
  receiver whose concrete class declares the method with `Code`? That converts
  1,405 rows into a retire/do-not-touch split with about 193 decisions.
  `MemorySegment` (87), `Path` (45), `XMLStreamReader` (43) and `ByteBuffer`
  (38) are the first four.
* **N2 — add the verdict to `shadow-triage.py`'s CSV** so no future lane
  re-derives it. The script already joins the registry; `image_declaring_method`
  is in the rows it reads and it currently ignores the
  `declared && !has_code && !acc_native` case.
* **N3 — mark the 21 `Path` slot-owners in the tree**, next to the two `H14-1`
  named. A comment at `nio_file.rs`'s registrar naming this record costs
  nothing and is the difference between a future lane retiring `Path.normalize`
  and not.
* **N4 — re-audit `H14-1`'s 156 *relocate* rows for the `Package.equals`
  shape** (§3.3): a deliberate override of an inherited method, where relocating
  the registration would delete a fix. The prescription is currently stated
  without exception.
* **N5 — do not quote `H14-3` row 11 as the price of retiring
  `register_phase57_nio_file`** (§3.1). It is the price of *arming its
  prefixes*, which also removed 21 `Path` implementations. A source retirement
  that excludes the no-`Code` rows has never been priced and would very likely
  cost less than 5 vectors.
