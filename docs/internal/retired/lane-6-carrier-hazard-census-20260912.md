# The unconstructed-carrier census — a species, not a bug

**Date:** 2026-09-12
**Origin:** [`lane-6-http-carrier-residuals-20260912.md`](lane-6-http-carrier-residuals-20260912.md) §3
**Gate:** `native-builtins/tests/unconstructed_carrier_gate.rs`
**Result:** 53 classes meet the precondition · 13 carry a non-zero declared
initialiser · **6 are confirmed** · none of the six is L6's

---

## 1. The species

Two decisions that are individually correct compose into a defect.

1. A retirement table entry deletes a native so the JDK's own bytecode runs
   instead. That is the point of `--jdk-only`, and it is usually right: the
   image's body is the reference implementation.
2. A native may hand back an instance it allocated itself, through
   `try_alloc_concurrent_synthetic`. In real-JDK mode that allocation is upsized
   to the real class's layout, so the object has every field the image declares,
   at the image's own slot indices.

What no step performs is the **constructor**. `try_alloc_concurrent_synthetic`
allocates; it does not run `<init>`, so no field initialiser the class declares
ever executes. Every field arrives as the zero of its type.

While the natives stay, nothing notices — the native bodies keep their state in
a side table and never read the fields. Retire one, and the JDK's own bytecode
reads them. **For any field whose declared initialiser is not zero, zero is a
legal value that means something else.**

## 2. The instance that started it

`java.net.HttpURLConnection`. `URL.openConnection()` allocates the carrier, and
the 2026-09-11 wave retired thirteen of its triples onto the image:

```
chunkLength             = -1     arrived 0
fixedContentLength      = -1     arrived 0
fixedContentLengthLong  = -1L    arrived 0
responseCode            = -1     arrived 0
method                  = "GET"  arrived null
```

`-1` means *unset*, so `0` read as SET. Both streaming setters refused on a
connection nobody had configured, each naming the mode the other had supposedly
set, and `getRequestMethod()` disagreed with what went on the wire. Five probe
rows across two sweeps, and every one looked like a separate defect until the
constructor was the answer.

`java.util.TreeMap` was the same species, found independently by another lane on
the same day ("TreeMap's declared reference fields were never written"). Two
sightings in one day is what makes it worth a census rather than a fix.

## 3. Method — three conditions, and only two are checkable in Rust

A class is at risk when **all three** hold:

1. **it is minted** — some native allocates it via `try_alloc_concurrent_synthetic`;
2. **it is retired** — it carries at least one triple in `native-api/src/retired_shadow.rs`,
   so the JDK's own bytecode runs on the object;
3. **it declares a non-zero initialiser that a retired method reads.**

Conditions 1 and 2 are source facts. Condition 3 is a question about a JDK image
and needs `javap`, which is why it lives in this record and not in the gate.

```
199   classes carrying retired triples          (tuple rows only)
408   classes minted by a native                (outside #[cfg(test)])
 53   BOTH  <- the gate's population
 13   ...also declare a non-zero initialiser    (javap -p -c, all constructors)
  6   ...where a RETIRED method reads that field
```

Two counting errors are recorded because each moved the answer by a lot:

* **A class NAMED in prose is not a retirement.** The first pass matched any
  quoted `a/b/C` string in `retired_shadow.rs` and reported 66. Restricting to
  `("class", "name", "descriptor"),` tuples gives 53.
* **A carrier minted in a test fixture is not minted.** The gate's first run
  reported eight extra classes — `java/lang/String`, `java/lang/Throwable`,
  `java/lang/Exception`, `java/util/Hashtable` among them — every one from
  `try_alloc_concurrent_synthetic(&mut ctx, ..).unwrap()` inside `mod tests`. No
  native mints `java.lang.String` in anger. The scan now skips `#[cfg(test)]`
  regions the way `lock_discipline_ratchet.rs` does.

The detector was validated before it was trusted: run against
`java.net.HttpURLConnection` it reproduces all five fields found by hand, plus
`URLConnection.doInput = true`, which the same wave had also had to write.

## 4. The thirteen

Fields whose declared initialiser is not the zero of their type.

| class | fields |
|---|---|
| `java/util/zip/ZipEntry` | `crc = -1L`, `csize = -1L`, `size = -1L`, `method = -1`, `externalFileAttributes = -1`, `csizeSet = true`, `xdostime = 2162688L` |
| `java/util/logging/LogManager` | `globalHandlersState = 1`, `initializationDone`, `initializedCalled`, `readPrimordialConfiguration` |
| `java/text/SimpleDateFormat` | `minusSign = '-'`, `serialVersionOnStream = 1`, `useDateFormatSymbols`, `hasFollowingMinusSign` |
| `java/lang/Thread` | `name`, `tid = 3L`, `interrupted` |
| `java/util/logging/Logger` | `isSystemLogger`, `anonymous` |
| `java/util/HashMap` | `loadFactor = 0.75f`, `threshold = 2147483647` |
| `java/util/TreeMap` | `size = 1`, `Entry.color = true` |
| `java/util/concurrent/ConcurrentHashMap` | `sizeCtl = 2147483647` |
| `java/util/ResourceBundle` | `expired = true` |
| `java/util/OptionalLong` | `isPresent = true` |
| `java/util/ArrayList$Itr` | `lastRet = -1` |
| `java/util/ArrayList$ListItr` | `lastRet = -1` |
| `java/nio/ByteBuffer` | `bigEndian = true` |

## 5. The six confirmed

A retired method on the class actually reads the field.

| class | field | read by | what a minted instance does |
|---|---|---|---|
| `java/nio/ByteBuffer` | `bigEndian` | `order` | reports **LITTLE_ENDIAN**; the JDK's default is BIG_ENDIAN |
| `java/util/OptionalLong` | `isPresent` | `getAsLong`, `ifPresent`, `isPresent`, `orElse` | reports **empty**; `getAsLong()` throws |
| `java/util/ArrayList$Itr` | `lastRet` | `remove` | `remove()` before `next()` **deletes element 0** instead of throwing `IllegalStateException` |
| `java/util/ArrayList$ListItr` | `lastRet` | `add`, `previous`, `set` | same, on three more methods |
| `java/util/logging/LogManager` | `globalHandlersState` | `readConfiguration`, `reset`, `updateConfiguration` | reconfiguration takes the wrong branch |
| `java/util/logging/Logger` | `isSystemLogger` | `log` | every logger looks like an application logger |

`ByteBuffer.bigEndian` is the one to read twice, because **it has already been
hit and patched, four times, one site at a time**: `set_field_by_name(buf,
"bigEndian", ..)` appears at four sites against six `try_alloc_concurrent_synthetic`
mint sites for that class. Somebody found this defect, fixed the instance in
front of them, and had no way to ask where else it applied. That gap is the
whole justification for the gate.

## 6. Whose rows these are

None of the six is L6's. That is the finding, not an evasion — `java/net/URL` is
in the population and is clean, and `HttpURLConnection` is already fixed at its
mint site.

| rows | lane |
|---|---|
| `ArrayList$Itr`, `ArrayList$ListItr`, `OptionalLong`, `HashMap`, `TreeMap`, `ConcurrentHashMap` | collections |
| `LogManager`, `Logger` | logging |
| `ByteBuffer` | nio |
| `SimpleDateFormat`, `ResourceBundle` | text / locale |
| `ZipEntry` | jar / zip |
| `Thread` | threading |

Each is a bounded change with a worked example to copy:
`huc_write_declared_field_defaults` in
`native-builtins/src/http_url_connection.rs` writes every declared initialiser
BY NAME at the mint site, pinning across allocations. **By name matters** — a
slot index depends on the image's field order, which is not this crate's to
assume.

## 7. The gate

`native-builtins/tests/unconstructed_carrier_gate.rs`, frozen at the 53. It
scores the PRECONDITION only, and it is a tripwire for a fifty-fourth, not a
verdict on the fifty-three. A new row is a request to run §3's narrowing before
landing, and its failure message says so in the order the work should be done.

Three tests: the population floors (a scan that stops matching passes every set
comparison while proving nothing); no new class in the intersection; and no
stale baseline row.

**It was proven to fail in both directions before it was trusted.** Deleting
`java/nio/ByteBuffer` from the baseline turns the first test red and names it;
adding a class that is not in the population turns the second red and names
that. A ratchet that has only ever been green is a ratchet nobody has tested.

The stale-row half is not bookkeeping. It is what fires when a scan is
accidentally blinded, and it did exactly that earlier the same day: the
predecessor wave moved thirty-one `r.register` calls into a macro, and
`registrar_drift.rs` reported eleven recorded pairs as "no longer drifting"
when every one of them was still real. Regenerating that baseline would have
destroyed eleven records and looked like a fix.

## 8. What this cannot see

* **Fields with a zero initialiser that the constructor computes anyway** — a
  `new HashMap<>()` inside `<init>` leaves a null the bytecode dereferences.
  That is the TreeMap lane's finding and it is a strictly larger species; this
  census answers only the non-zero *literal* initialiser.
* **Carriers minted with a class name held in a variable.** The scan takes the
  first string literal after the context argument, so a computed class name
  yields nothing. Under-reporting is the deliberate direction.
* **Whether the retired method is ever reached** with a minted receiver. All six
  confirmed rows are reachable in principle; only `HttpURLConnection`'s were
  measured by a probe.
* **`--dump-native-registry` cannot answer any of this.** A retired triple is
  one that is NOT in the registry, so the dump's silence about a class is
  indistinguishable from the class never having been registered. The tables are
  the only place retirement is written down.
