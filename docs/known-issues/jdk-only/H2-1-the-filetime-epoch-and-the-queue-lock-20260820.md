# H2-1 — the FILETIME epoch, the queue's third field, and why `java/lang/ref/` is not a state problem

**Status** `FIXED-UNVERIFIED` — **no binary carrying these changes has been
built or run.** Every "after" figure below is labelled **PREDICTED** and each
one says what would falsify it. Nothing here is MEASURED except the JDK 25
source and class metadata quoted in §2, which were read on this host.

**Date** 2026-08-20
**Subject** `G90-1` §8 N2. `native-builtins/src/phases_late/nio_file.rs`,
`native-builtins/src/reference.rs`, `native-api/src/retired_shadow.rs`
**Oracle read** HotSpot 25.0.3+9-LTS at
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`
**Acceptance, not yet run** `regression-suite/run.sh`,
`CRATONVM_ARGS=--jdk-only`, 102 vectors. The three checks in `G90-1` §5 are the
criteria and they were written before this work started.

> **A correction to the handoff, because the next lane will lose the same ten
> minutes.** `HANDOFF-20260819.md` and this directory's records give the JDK as
> `C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`. **That path does not
> exist on this host.** The 25.0.3+9 image is at
> `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`, `where java` resolves to
> it, and `$JAVA_HOME` already points at it. Its `lib/src.zip` is 52 465 101
> bytes and both entries used below are in it.

---

> **VERIFIED AGAINST A BINARY 2026-09-04. All three rows of §"`G90-1` §5 check"
> hold, including the two that were predicted NOT to move.** Status was
> *"`FIXED-UNVERIFIED` — no binary carrying these changes has been built or
> run"*, with the acceptance run named and not yet done.
>
> ```text
> vector                    CK lines   differing from HotSpot   verdict
> RFileTimes                   69              0                PASS (68 checks)
> RClassUnloadSweep             1              0                PASS
> RClassUnloadSweepGen          1              0                PASS
> ```
>
> **Row 1 — predicted to flip, and it flipped.** The check this record is named
> for now reads identically on both VMs:
>
> ```text
> HotSpot   CK plain.readAttributes.lastModified 2021-01-01T00:00:00Z
> CratonVM  CK plain.readAttributes.lastModified 2021-01-01T00:00:00Z
> ```
>
> Its stated falsifier is *"any surviving 1601 date"*. The string `1601` occurs
> **zero** times anywhere in the `--jdk-only` transcript. The record's second
> falsifier — *"any other wrong instant"*, which would mean `MetadataExt`
> disagrees with `GetFileAttributesEx` rather than an epoch bug — also does not
> fire: all 69 `CK` lines match, not just this one.
>
> **Rows 2 and 3 — predicted NOT to flip, and their falsifier is appearing in
> the failing set at all.** Neither appears. Both pass in Compatible and
> `--jdk-only` in a full 129-vector run and byte-match the oracle when run
> alone. §4.1's reasoning stands: retiring the prefix removed the VM's only
> reference-discovery hook, so `java/lang/ref/` is not retired here, and §4.2's
> state writes did not change weak-reference behaviour.
>
> This record's own summary — *"predicted to close one of the three, and to
> explain rather than close the other two"* — is what happened.
>
> **What this does NOT verify.** The acceptance line says *"102 vectors"*; the
> suite is now **129**, so the total is not comparable with any 102-vector
> baseline and no such comparison is made — only the three named vectors are
> adjudicated. §2's JDK 25 source and class metadata are oracle readings and
> were not re-derived. The `MetadataExt` / `GetFileAttributesEx` distinction is
> a Windows path; this run is Linux, so row 1's *encoding* fix is confirmed by
> its observable and not by exercising that code path.

## 1. The two diffs, and what each one turned out to be

`G90-1` §5 armed `CRATONVM_ENFORCE_NATIVE_SHADOW` on seven prefixes and the
102-vector arm rejected two of them:

```text
  RFileTimes            plain.readAttributes.lastModified
                          HotSpot   2021-01-01T00:00:00Z
                          CratonVM  1601-01-02T20:42:25.920Z
  RClassUnloadSweep     payload.class.unloaded
  RClassUnloadSweepGen    HotSpot true / CratonVM false
```

They look like one finding — "the VM owns state that belongs to the real
object" — and they are two different things.

**`RFileTimes` is exactly what it looks like, and the arithmetic closes.**
1609459200000 is 2021-01-01T00:00:00Z in Unix-epoch millis. Read as
100-nanosecond ticks since 1601-01-01 that is 160 945.92 seconds, i.e.
**1601-01-02T20:42:25.920Z** — the printed value, to the millisecond. The VM was
writing millis into fields the real class reads as FILETIME. §2 and §3.

**`RClassUnloadSweep` is not a `Reference`-state problem, and it is not a
class-unloading problem either.** It is a *discovery* problem, one level
upstream of both, and no amount of field population fixes it. §4.

---

## 2. What the JDK 25 sources actually say

Read on this host, 2026-08-20. Field names, types and encodings below are quoted
from these two entries and cross-checked against the compiled classes with
`javap -p`, which is the authority on declaration order and descriptors.

```bash
JDK="/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot"
unzip -p "$JDK/lib/src.zip" java.base/sun/nio/fs/WindowsFileAttributes.java
unzip -p "$JDK/lib/src.zip" java.base/java/lang/ref/Reference.java
unzip -p "$JDK/lib/src.zip" java.base/java/lang/ref/ReferenceQueue.java
"$JDK/bin/javap" -p sun.nio.fs.WindowsFileAttributes
"$JDK/bin/javap" -p java.lang.ref.Reference
"$JDK/bin/javap" -p java.lang.ref.ReferenceQueue
"$JDK/bin/javap" -p 'java.lang.ref.ReferenceQueue$Lock'
```

### 2.1 `java.base/sun/nio/fs/WindowsFileAttributes.java`

Instance fields, in declaration order (`javap -p`, so this is the class file's
own order, not the source's):

```text
  private final int  fileAttrs;
  private final long creationTime;
  private final long lastAccessTime;
  private final long lastWriteTime;
  private final long size;
  private final int  reparseTag;
  private final int  volSerialNumber;
  private final int  fileIndexHigh;
  private final int  fileIndexLow;
```

The three times are **Windows FILETIME**: 100-nanosecond intervals since
1601-01-01T00:00:00Z UTC. The class states its own conversion:

```java
  private static final long WINDOWS_EPOCH_IN_100NS  = -116444736000000000L;

  static FileTime toFileTime(long time) {
      long adjusted = Math.addExact(time, WINDOWS_EPOCH_IN_100NS);
      long nanos = Math.multiplyExact(adjusted, 100L);
      return FileTime.from(nanos, TimeUnit.NANOSECONDS);
  }
  static long toWindowsTime(FileTime time) {
      long adjusted = time.to(TimeUnit.NANOSECONDS)/100L;
      return adjusted - WINDOWS_EPOCH_IN_100NS;
  }
```

The predicates, which matter as much as the times because they read a *different*
field than CratonVM's natives did:

```java
  public boolean isSymbolicLink() { return reparseTag == IO_REPARSE_TAG_SYMLINK; }
  public boolean isDirectory()    { if (isSymbolicLink()) return false;
                                    return ((fileAttrs & FILE_ATTRIBUTE_DIRECTORY) != 0); }
  public boolean isOther()        { if (isSymbolicLink()) return false;
                                    return ((fileAttrs & (FILE_ATTRIBUTE_DEVICE
                                            | FILE_ATTRIBUTE_REPARSE_POINT)) != 0); }
  public boolean isRegularFile()  { return !isSymbolicLink() && !isDirectory() && !isOther(); }
  public boolean isReadOnly()     { return (fileAttrs & FILE_ATTRIBUTE_READONLY) != 0; }
  public Object  fileKey()        { return null; }
```

`java.base/sun/nio/fs/WindowsConstants.java`:
`IO_REPARSE_TAG_SYMLINK = 0xA000000C`, `FILE_ATTRIBUTE_REPARSE_POINT = 0x400`,
`FILE_ATTRIBUTE_DIRECTORY = 0x10`, `FILE_ATTRIBUTE_DEVICE = 0x40`,
`FILE_ATTRIBUTE_READONLY = 0x1`, `HIDDEN = 0x2`, `SYSTEM = 0x4`, `ARCHIVE = 0x20`.

**`isSymbolicLink()` never looks at `FILE_ATTRIBUTE_REPARSE_POINT`.** CratonVM's
native did, and only that. A carrier with the bit set and `reparseTag == 0` reads
back through real bytecode as `isOther() == true`, `isSymbolicLink() == false`.

### 2.2 `java.base/java/lang/ref/Reference.java`

```text
  private T referent;                                  // slot 0
  volatile ReferenceQueue<? super T> queue;            // slot 1
  volatile Reference next;                             // slot 2
  private transient Reference<?> discovered;           // slot 3
```

```java
  Reference(T referent, ReferenceQueue<? super T> queue) {
      this.referent = referent;
      this.queue = (queue == null) ? ReferenceQueue.NULL_QUEUE : queue;
  }
  public T get()             { return this.referent; }
  public boolean isEnqueued() { return (this.queue == ReferenceQueue.ENQUEUED); }
  public boolean enqueue()   { clearImpl(); return this.queue.enqueue(this); }
```

`clear()`/`refersTo()` bottom out in `clear0()`/`refersTo0()`, both
`private native @IntrinsicCandidate` — so they are §1.5 bridges, not §1.4
shadows. CratonVM registers both (`native-builtins/src/lib.rs`, `clear0` and
`refersTo0`, on `Reference` and on `PhantomReference`).

**`queue` is never null on a real `Reference`.** `enqueue()` is a bare
dereference of it.

### 2.3 `java.base/java/lang/ref/ReferenceQueue.java`

```text
  private volatile Reference<? extends T> head;   // slot 0
  private long queueLength = 0;                   // slot 1   <- LONG
  private final Lock lock = new Lock();           // slot 2
```

with `private static class Lock { }` — a plain object, not a
`java.util.concurrent` lock (`javap -p 'java.lang.ref.ReferenceQueue$Lock'`
shows one private no-arg constructor and no fields). `enqueue`, `poll`,
`remove()` and `remove(long)` each open with `synchronized (lock)`, and
`remove0` blocks in `lock.wait()`.

```java
  static final ReferenceQueue<Object> NULL_QUEUE = new Null();
  static final ReferenceQueue<Object> ENQUEUED   = new Null();

  private boolean enqueue0(Reference<? extends T> r) {      // must hold lock
      ReferenceQueue<?> queue = r.queue;
      if ((queue == NULL_QUEUE) || (queue == ENQUEUED)) return false;
      r.next = (head == null) ? r : head;                   // SELF-LOOP end marker
      head = r; queueLength++;
      r.queue = ENQUEUED;
      lock.notifyAll();
      return true;
  }
  private Reference<? extends T> poll0() {                  // must hold lock
      Reference<? extends T> r = head;
      if (r != null) {
          r.queue = NULL_QUEUE;
          Reference<? extends T> rn = r.next;
          head = (rn == r) ? null : rn;
          r.next = r;                                       // self-loop, not null
          queueLength--;
          return r;
      }
      return null;
  }
  public Reference<? extends T> poll() {
      if (head == null) return null;
      ...
      synchronized (lock) { return poll0(); }
  }
```

**`lock` is a FIELD INITIALISER**, which means the real `<init>` is the only
thing that creates it — and `native_rq_init` replaces that constructor. A queue
this VM built and then handed to real bytecode monitors a null.

---

## 3. Part 1 — `sun/nio/fs/`

### What was wrong

`basic_file_attributes_store` (`native-builtins/src/phases_late/nio_file.rs`)
wrote Unix-epoch millis into `creationTime`/`lastAccessTime`/`lastWriteTime`,
and `basic_file_attributes_time_millis` read them back the same way. Perfectly
self-consistent, and agreeing with nothing — the shape `unix_attr_time_fields`'
own doc comment describes for the Linux carrier's *names*, reappearing here as a
wrong *unit*. It also wrote `fileAttrs` as `if is_dir { 0x10 } else { 0 }` and
never wrote `reparseTag` at all.

### What changed

1. **FILETIME encoding.** New `WINDOWS_EPOCH_IN_100NS`, `win_filetime_from_millis`
   and `win_millis_from_filetime`, each quoting the JDK's own arithmetic.
   `basic_file_attributes_store`'s Windows arm converts on the way in;
   `basic_file_attributes_time_millis`'s Windows arm converts on the way out.
   Our own accessors see no change; the real bytecode sees the right value.
2. **The real DOS attribute word.** In `p59_files_read_attributes`, a
   `#[cfg(windows)]` block writes `meta.file_attributes()` (the raw
   `dwFileAttributes` off `std::os::windows::fs::MetadataExt`) into `fileAttrs`,
   and the raw `creation_time()`/`last_access_time()`/`last_write_time()` — which
   `std` documents as FILETIME — into the three time fields, skipping any that
   the OS reports as 0. That gives real `isReadOnly`/`isHidden`/`isArchive`/
   `isSystem` bytecode something to test, and drops the millis rounding.
3. **`reparseTag`.** Written as a definite `0` by the store, and raised to
   `IO_REPARSE_TAG_SYMLINK` in the symlink arm of `p59_files_read_attributes`
   alongside the existing `FILE_ATTRIBUTE_REPARSE_POINT` bit.
4. Every write is **by name**. No slot index is used against the real layout
   anywhere in this change; see `docs/architecture/natives-over-real-jdk-classes.md`
   §5 for why that distinction is heap corruption rather than a wrong answer.

### What it does NOT do

* **It does not touch the Unix carrier, and it could not verify it.** This
  host's `src.zip` contains **no `sun/nio/fs/Unix*.java` at all** — a Windows
  JDK build ships only its own platform sources, and
  `unzip -l "$JDK/lib/src.zip" | grep 'sun/nio/fs/.*Attributes'` returns exactly
  one line, `WindowsFileAttributes.java`. So the claim "the Unix side is already
  in the JDK's own encoding" is **source-unverified here**. What I can say is
  in-tree and checkable: `unix_attr_store_time`/`unix_attr_time_fields` already
  write the split `st_mtime_sec`/`st_mtime_nsec` pairs and `st_mode`, and their
  doc comment records a Linux measurement (`jarmode-tools-extract-timestamp-preservation-FIXED.md`) that closed exactly
  the analogous defect for the *names*. No FILETIME-shaped unit bug can exist
  there, because seconds-plus-nanos is the encoding the real class uses. That is
  an argument, not a measurement.
* It does not change `fileKey()`, and does not retire it (§5).
* It does not touch `java/nio/file/Files.readAttributes`, which still runs as a
  native and still builds the carrier. That is the design: the native produces
  the object, the real bytecode reads it.
* It does not populate `volSerialNumber`/`fileIndexHigh`/`fileIndexLow` for
  ordinary files — the existing "directories and links only" restriction is
  unchanged, because each one costs a `CreateFile`.

---

## 4. Part 2 — `java/lang/ref/`

### 4.1 The `RClassUnloadSweep` half is a different defect, and here is the evidence

`RClassUnloadSweep`'s only observable is a `WeakReference<Class<?>>` clearing
inside twelve `System.gc()` rounds. The obvious hypothesis is that
`Reference.get()` yields to real bytecode and reads a field CratonVM never
nulls. **That is not it.** Real `get()` is `return this.referent;`, CratonVM's
native reads slot 0, and slot 0 *is* `referent` on the real layout (§2.2) — the
two agree.

The actual mechanism, source-verified 2026-08-20:

```text
  grep -rn "discover_reference" --include=*.rs .
```

Every mutator-side call is in `native-builtins` (and `native-io`), and the ones
for weak/soft/phantom are in `reference.rs`'s **constructors** —
`discover_ref_from_args`, called from `native_weak_ref_init`,
`native_soft_ref_init`, `native_phantom_ref_init` and their queue overloads. The
only other producers are `SharedVm::register_finalizable` (finalizers) and the
`Cleaner` paths in `phases_late.rs` / `servlet.rs` / `native-io`. **Nothing in
`gc/` scans the heap for `java.lang.ref.Reference` instances.** The reference
processor is a *registry*, and the registration hook is the constructor native.

So retiring `java/lang/ref/` retires `Reference.<init>` and the three subclass
constructors along with everything else — and a reference whose constructor
yielded to real bytecode is never discovered, never cleared, and
`payload.class.unloaded` reads `false`. That is what the arm saw. It is not a
field-population defect and populating fields does not move it.

Retiring only the accessors is not the escape hatch it looks like. Three of them
carry VM work the real bytecode has no equivalent for, each with a named in-tree
defect behind it:

| triple | what retirement would delete | the record |
|---|---|---|
| `Reference.get()` | `gc_reference_keep_alive` — the SATB pre-barrier that keeps a referent handed to the mutator alive for the rest of a G1 concurrent cycle | INT-8, in-file |
| `SoftReference.get()` | `touch_soft_reference` — without it every soft ref looks infinitely idle | Round-5 / Groovy `MetaClassImpl.addFields`, in-file |
| `Reference.enqueue()` | `mark_reference_manually_enqueued` | PGJDBC-PHANTOM-GHOST 2026-08-07, in-file |
| `ReferenceQueue.poll()` | the self-link normalisation that the GC's auto-enqueue convention needs | PGJDBC-PHANTOM-DOUBLE-POLL 2026-08-07, in-file |

**Verdict: `java/lang/ref/` is not retired, and it is not a candidate until the
GC-side auto-enqueue and discovery move into the real object model.** That is a
`native-collections`-sized migration, not a table entry. See §7 N1.

### 4.2 What did change — the state, so a later retirement has a floor to stand on

1. **`ReferenceQueue.<init>` now creates the real `lock`.** `native_rq_init`
   grew a real-layout arm: `head = null`, `queueLength = 0L` (a `long`, by name,
   where a raw `Int(0)` slot write used to go), and a raw allocation of
   `java/lang/ref/ReferenceQueue$Lock` written into `lock`. Allocation without
   running the constructor is deliberate: `Lock` declares no fields and its
   private no-arg constructor has an empty body, so the object is identical and
   nothing has to invoke a private constructor across the native boundary. `this`
   is pinned across `ensure_class_initialized` and `alloc_object`, both of which
   are GC-capable, and re-read afterwards; nothing allocates between the
   allocation and the write, so the fresh `Lock` needs no pin of its own.
2. **`Reference.queue` now holds `NULL_QUEUE`, not null.** `ref_init_impl`'s
   real-layout arm substitutes `ReferenceQueue.NULL_QUEUE` for an absent queue,
   as the real constructor does. `native_rq_poll` detaches the same way, where it
   used to write a raw null — its own comment already admitted the divergence.
   Both resolutions go through a new `reference_queue_null_sentinel`, built
   exactly like the existing `reference_queue_enqueued_sentinel`:
   `class_id_by_name` rather than `ensure_class_initialized`, so it is
   **non-GC-capable** and the unpinned `ObjectRef`s its callers hold cannot move
   under it. When the class is not yet initialised it answers `None` and both
   sites fall back to today's behaviour rather than inventing a value.
3. **`native_ref_enqueue` short-circuits `NULL_QUEUE`.** Its "no queue" test was
   `queue == null`, which the change above would have silently broken. It now
   also compares against the sentinel and returns `false` without taking any
   lock — the same verdict real `enqueue0` reaches the long way round.
4. Two comments corrected against the source: `with_queue_monitor` said the JDK
   guards the queue with a `ReentrantLock`; it is a private `Lock` object
   monitored with `synchronized`/`wait`/`notifyAll`. The lock-order argument the
   comment exists for is unaffected — what it needs is "never `this`", and that
   is still true — but the fact was wrong. The module header now records the real
   four- and three-field layouts beside the synthetic two-slot ones.

### What Part 2 does NOT do

* **It does not adopt the JDK's self-loop `next` convention**, and that is a
  decision rather than an oversight. Real `enqueue0`/`poll0` mark the end of the
  list by self-linking (`r.next = r`); CratonVM's GC-side auto-enqueue uses the
  other convention (`next` = old head, null when empty) and lives outside
  `native-builtins`. Changing one end of a two-producer/one-slot contract without
  the other is precisely the hazard `native_rq_poll`'s existing normalisation
  comment describes. §7 N2.
* It does not add a lock. The `ReferenceQueue$Lock` object is a Java monitor on
  a per-queue object, not a Rust `Mutex` and not a new global — the
  native-builtins lock ratchet has nothing to count.
* It does not make `Reference` state real in the sense the assignment asked
  about. `referent` and `queue` were already in the real fields; `next` and
  `discovered` are shared with a GC path this lane does not own.
* **It does not widen the set of addresses the reference processor writes
  through**, and this is worth stating explicitly because the memory note about
  `num_fields >= 2` shape-checking is a live hazard here. `discover_reference` is
  called from exactly the same four constructors, with exactly the same
  arguments, as before this change. No new object becomes a reference-processor
  subject. The guard that should still be tightened is nominated in §7 N3
  regardless — it is not made worse or better by this work.

---

## 5. Part 3 — the retirement, and the judgement behind it

**Retired: eight `sun/nio/fs/WindowsFileAttributes` triples. Not retired:
`java/lang/ref/`, and `WindowsFileAttributes.fileKey`.**

```text
  sun/nio/fs/WindowsFileAttributes.creationTime     ()Ljava/nio/file/attribute/FileTime;
  sun/nio/fs/WindowsFileAttributes.lastAccessTime   ()Ljava/nio/file/attribute/FileTime;
  sun/nio/fs/WindowsFileAttributes.lastModifiedTime ()Ljava/nio/file/attribute/FileTime;
  sun/nio/fs/WindowsFileAttributes.isDirectory      ()Z
  sun/nio/fs/WindowsFileAttributes.isOther          ()Z
  sun/nio/fs/WindowsFileAttributes.isRegularFile    ()Z
  sun/nio/fs/WindowsFileAttributes.isSymbolicLink   ()Z
  sun/nio/fs/WindowsFileAttributes.size             ()J
```

Plus `"sun/nio/fs/"` in `RETIRED_SHADOW_PREFIXES` — the narrow prefix
deliberately, not `sun/nio/`, because `sun/nio/ch/` scored 34/36 on the
2026-08-19 dial sweep.

**Why retire rather than hide behind a switch.** The assignment's own reasoning
holds: a default-OFF knob makes the orchestrator's next arm run measure the old
behaviour, which is a 20-minute run that proves nothing. Retiring makes the run
either confirm the fix in one shot or produce a diff that names its own cause,
which is how `RFileTimes` earned its diagnosis in the first place. The state
population (§3, §4.2) and the retirement (§5) are **two commits** so the
retirement reverts alone; the exact revert is named in §8.

**`fileKey` is held, and not out of caution.** JDK 25
`WindowsFileAttributes.fileKey()` is `return null;` — the Windows provider has
no file identity. CratonVM's native answers a real `(volume, index)` key from
`GetFileInformationByHandle`, which is what lets `FileTreeWalker.wouldLoop` see a
symlink cycle on this platform. Retiring it would be HotSpot-identical and
strictly worse behaviour, and it is not a state-population question at all:
there is no state the real body would read. Same shape as `Logger.log`'s eighth
overload in the first table — the reason that list is per-TRIPLE.

**`sun/nio/fs/UnixFileAttributes` gets no entries.** The same nine methods are
registered on it, and the 2026-08-19 census held them under *class never loaded
in 36 vectors*. `G88-1`'s rule is that `class-not-loaded` is the absence of a
verdict, not a clean one, and §3's Unix argument is an argument. Adding them
would be exactly the "wider claim than the measurement" defect this wave was
named after. `the_unix_attribute_carrier_is_not_retired` pins it.

**Gates changed in `retired_shadow.rs`:**

| gate | change |
|---|---|
| `the_stateless_table_stays_inside_the_five_measured_prefixes` | renamed to `..._inside_the_measured_prefixes`; sixth prefix `sun/nio/fs/` admitted, with the doc saying in as many words that it is the one entry **not** backed by an arm run |
| `the_stateless_table_is_not_empty` | floor 220 → 235, message re-derived (227 + 8) |
| `a_prefix_alone_retires_nothing` | the `sun/nio/fs/WindowsFileAttributes.creationTime` negative became a positive elsewhere, so its place is taken by `notAMethod` and `sun/nio/fs/WindowsPath.toString` — the prefix now admits the package and must earn its `false` from the table |
| `the_reference_subsystem_stays_whole` | **new.** Eleven `java/lang/ref/` triples asserted not-retired, with §4.1's reason in the failure message |
| `the_held_windows_attribute_triple_is_not_retired` | **new.** `fileKey` held, and one of the eight asserted retired so the test cannot pass on an empty table |
| `the_unix_attribute_carrier_is_not_retired` | **new.** |

---

## 6. What I expect to flip, and what I do not

All **PREDICTED**. I built nothing and ran nothing.

| `G90-1` §5 check | prediction | why | what falsifies it |
|---|---|---|---|
| `RFileTimes plain.readAttributes.lastModified` | **flips to matching HotSpot** | the encoding was the whole defect and the arithmetic closes to the millisecond (§1) | any surviving 1601 date — that would mean the field is being written somewhere this change does not reach. Any *other* wrong instant means the FILETIME source (`MetadataExt`) disagrees with `GetFileAttributesEx`, not the epoch |
| `RClassUnloadSweep payload.class.unloaded` | **does NOT flip, and is not expected to** | §4.1 — retiring the prefix removed the VM's only reference-discovery hook, and `java/lang/ref/` is therefore not retired here. With the prefix absent this check should simply stay at the 102/102 baseline value, i.e. it should not appear in the failing set at all | it appearing in the failing set at all. That would mean §4.2's state writes changed weak-reference behaviour, which they are not supposed to do |
| `RClassUnloadSweepGen` | same | same vector under the generational collector | same |

**The honest summary of that table is that this change is predicted to close one
of the three, and to explain rather than close the other two.** `G90-1` §8 N2
asked for both prefixes; one of them turned out to be the wrong request.

Two more predictions worth writing down so they can be falsified:

* **The whole-suite verdict is predicted verdict-NEUTRAL, not green.**
  `HANDOFF-20260819.md` §6: `--jdk-only` 102/102, `SUITE=all` 97/102 with a known
  five, `SUITE=core` 61/62. A change that leaves the same five failing for the
  same reasons is the acceptance criterion.
* **Stub ratchet: `+8` in both configurations, registry rows UNCHANGED.**
  `BASELINE_SYNTHETIC_STUBS_MANAGEMENT` 1622 → **1630**,
  `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT` 1611 → **1619**;
  `MEASURED_TOTAL_REGISTRATIONS` 13160 / 12792 unchanged. Eight triples, one
  registering class (`sun/nio/fs/WindowsFileAttributes`), so eight rows change
  kind and none is added. `register_p59_file_attributes` is reached from BOTH
  `register_phase59_natives` and `reflect_annotations.rs`, and `register()` is
  last-registration-wins, so the second call updates the same slot rather than
  adding one. A row-count movement of any size falsifies this and means the
  change added a fake rather than relabelling one — `G89-1` §2's two-column rule
  reading its second real case.

---

## 7. NOMINATIONS

* **N1 — move reference DISCOVERY out of the constructor native.** This is the
  single thing standing between `java/lang/ref/` and retirement, and §4.1 is its
  evidence. Today `ctx.discover_reference` is called only from
  `native-builtins`' `Reference` subclass constructors, so the reference
  processor cannot see a reference the VM did not construct through a native.
  The shape that would fix it is a marking-time discovery pass keyed on
  "instance of a `java.lang.ref.Reference` subclass", which is what HotSpot
  does. Sized like the `native-collections` side-state migration (`G88-1` N3),
  and blocked behind the same kind of decision.
* **N2 — two producers, one `next` slot.** CratonVM's GC-side auto-enqueue uses
  `next = old head, null when empty`; the JDK uses a self-loop end marker.
  `native_rq_poll` normalises one into the other at read time and
  `native_ref_enqueue`'s real-layout arm delegates to real bytecode that writes
  the other. Pick one convention and make both ends write it — the read-time
  normalisation is a tag inferred from a convention, which is exactly the shape
  the memory note about two producers and one slot warns about.
* **N3 — tighten the reference processor's `num_fields >= 2` shape check.**
  Unchanged by this work and still live: the processor writes through addresses
  it only shape-checks, and `num_fields >= 2` admits `String`. The guard should
  test the receiver's class against the `java.lang.ref.Reference` hierarchy, not
  its arity. The witness signatures are a `ClassCastException` out of
  `ReferenceQueue.poll()` and an NPE naming `<localN>`. This becomes more
  important, not less, if N1 lands.
* **N4 — `sun/nio/fs/UnixFileAttributes` needs a Linux arm.** Nine more triples
  are sitting behind a `class-not-loaded` verdict from a Windows census. One
  `regression-suite/run.sh` run on the Azure Linux host with
  `CRATONVM_ENFORCE_NATIVE_SHADOW=sun/nio/fs/` decides them, and §3 predicts it
  passes.
* **N5 — the `WindowsFileAttributes` DOS predicates were never registered at
  all.** `isReadOnly`/`isHidden`/`isArchive`/`isSystem` have no native on the
  concrete class, so before this change they read a `fileAttrs` word that only
  ever carried `FILE_ATTRIBUTE_DIRECTORY` — `false` for every file on disk,
  silently. §3 item 2 fixes the data; nobody has ever measured the answer. A
  `CK dos.readonly` line in `RFileTimes` would close it.
* **N6 — correct the JDK path in `HANDOFF-20260819.md`.** Orchestrator-owned;
  see the callout at the top. It costs every lane the same ten minutes.

---

## 8. OUT-OF-FILE EDITS REQUIRED

I own four files and made no edit outside them. These are the ones I could not
make.

### 8.1 `vm/src/runtime/interpreter/native_override.rs` — VERIFY FIRST, then narrow

**This is the one that can make §5 inert, and I could not test it.**

`force_native_over_real_jdk_bytecode` (declared at
`vm/src/runtime/interpreter/native_override.rs:2545`) contains an arm keyed on
the *interface*, at **`native_override.rs:3340-3359`** (anchor on the quoted
text, not the line numbers — this file is being edited concurrently). Current
text:

```rust
    // File-attribute values are carried by a private five-slot synthetic
    // object, not by the real JDK's zero-field interface or platform-private
    // attribute layouts. Interface call sites must therefore dispatch to the
    // registered bridge before any receiver-class bytecode is selected.
    if class_name == "java/nio/file/attribute/BasicFileAttributes"
        && matches!(
            (method_name, method_descriptor),
            ("creationTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("lastAccessTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("lastModifiedTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("isDirectory", "()Z")
                | ("isRegularFile", "()Z")
                | ("isSymbolicLink", "()Z")
                | ("isOther", "()Z")
                | ("size", "()J")
                | ("fileKey", "()Ljava/lang/Object;")
        )
    {
        return true;
    }
```

**The question, which one run answers.** `RFileTimes` reaches these through
`BasicFileAttributes attrs = view.readAttributes(); attrs.lastModifiedTime()` —
an `invokeinterface` on a receiver whose class is `sun/nio/fs/WindowsFileAttributes`.
If dispatch resolves the *receiver class's* concrete method, the retirement in
§5 takes effect and this arm never sees the triple. If it resolves the
*interface* declaration, this arm forces the still-live interface-keyed native
and **the retirement is a silent no-op** — the arm run would come back 102/102
having measured nothing, which is the failure mode this whole directory is
about.

Verify with one command on a binary carrying this branch:

```bash
CRATONVM_DBG_DROPPED_STUBS=1 cratonvm --java-home "$JDK" --jdk-only \
  -cp regression-suite/build RFileTimes 2>&1 | grep -i 'WindowsFileAttributes'
```

A `[JDK-ONLY-REFUSED]` line naming
`sun/nio/fs/WindowsFileAttributes.lastModifiedTime` means the retirement is
live and no edit is needed. **Silence means this arm is intercepting**, and the
replacement is to make the arm state its real premise — that it exists for the
SYNTHETIC five-slot carrier, whose class *is* the interface, and not for a real
platform layout:

```rust
    // File-attribute values are carried by a private five-slot synthetic
    // object whose CLASS IS the interface (`BasicFileAttributes` is abstract in
    // every real JDK, so no genuine instance can carry that class id). That
    // carrier has no bytecode and needs the bridge. A real
    // `sun.nio.fs.{Unix,Windows}FileAttributes` receiver does NOT: as of
    // 2026-08-20 (H2-1) its fields are populated in the JDK's own encodings, and
    // eight of its nine accessors are retired §1.4 shadows. Forcing the bridge
    // for those receivers reinstates exactly the shadow the retirement dropped.
    if class_name == "java/nio/file/attribute/BasicFileAttributes"
        && receiver_class_name == "java/nio/file/attribute/BasicFileAttributes"
        && matches!(
```

`receiver_class_name` is a placeholder for whatever this function is given for
the receiver — **it may not have one**, in which case the narrowing has to move
to the call site rather than into this predicate, and that is a design question
for whoever owns the dispatch chain, not a patch I can write blind. Do not apply
this hunk without checking the signature at `native_override.rs:2545`.

### 8.2 `native-builtins/tests/stub_ratchet.rs` and `vm/tests/stub_ratchet.rs`

Owned by another lane this round. After the arm confirms §5, the two frozen
constants move by exactly `+8`:

* `native-builtins/tests/stub_ratchet.rs:768` — current
  `const BASELINE_SYNTHETIC_STUBS_MANAGEMENT: usize = 1622;`
  → `const BASELINE_SYNTHETIC_STUBS_MANAGEMENT: usize = 1630;`
* `native-builtins/tests/stub_ratchet.rs:773` — current
  `const BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT: usize = 1611;`
  → `const BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT: usize = 1619;`

`MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT` (`:790`, 13160) and
`..._NO_MANAGEMENT` (`:793`, 12792) must **not** move. **Do not hand-edit these
from this document** — the file's own header says to run the test and paste the
`stub-ratchet: const <BASELINE_CONST>: usize = <N>;` line it prints. `1630` and
`1619` are a prediction to check the printed number against, not a value to
install.

### 8.3 `scripts/baselines/jdk-only-bridge-ratchet.json`

Owned by another lane. The `bridge_*` counts fall by up to 8 on a re-census.
That entry already carries a `PENDING RE-FREEZE, NOT APPLIED, 2026-08-12` note
saying it is stale and will fire; this change makes it staler in the same
direction. No hand edit — its own note explains why a hand-written count that
lands too high widens a slack-free ratchet.

### 8.4 `docs/known-issues/jdk-only/INDEX.md`

Orchestrator-owned. Needs a row for this record: `H2-1`, status
`FIXED-UNVERIFIED`, provenance **PREDICTED**, subsystem *nio / ref / shadow
retirement*.

### 8.5 `docs/known-issues/jdk-only/G90-1-…-20260819.md` §8 N2

Orchestrator-owned. N2 asks for both prefixes to be armed. §4.1 says one of them
cannot be, and why. N2 should be split: the `sun/nio/fs/` half is done pending
an arm; the `java/lang/ref/` half is §7 N1 and is not a state-population task.

---

## 9. What would make me wrong

* **The interface-dispatch question in §8.1.** If that arm intercepts, §5 is
  inert and the arm run measures nothing. That is the same "population narrower
  than the claim" defect `G90-1` §5 is a monument to, and it is why it is the
  first out-of-file item rather than the last.
* **`meta.file_attributes()` on a path opened through the VFS/jar sentinel.**
  The `#[cfg(windows)]` block runs only in the `Ok(meta)` arm of a real
  `std::fs::metadata`, and the jar-FS / jrt-FS paths return before it — but the
  guard is the enclosing control flow, not an assertion. A jar entry that
  somehow reached it would get a DOS attribute word for the wrong file.
* **§3's Unix argument.** It is an argument. `src.zip` on a Windows JDK cannot
  settle it, and §7 N4 is the run that can.
* **The `ReferenceQueue$Lock` raw allocation.** `alloc_object` with zero fields
  produces the object shape `RClassUnloadSweep` exists to stress. Its class id is
  not `ClassId(0)`, so the header is not all-zero and the sweep's `word0 != 0`
  proxy is not affected — but that reasoning is inspection, and the vector is the
  instrument.

## 10. The revert

Two commits in this lane's worktree, in this order:

1. the state population — `nio_file.rs` + `reference.rs`;
2. the retirement — `retired_shadow.rs` alone.

Reverting **(2)** alone restores the pre-2026-08-20 shadow set with every state
fix still in place, which is the configuration to fall back to if the arm
regresses: the FILETIME encoding is right in compatible mode too, and the queue
lock is a bug fix independent of any retirement.
