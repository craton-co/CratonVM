> **j27 wave 8 (lane W8DSP, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION (HotSpot-only measurement).**
> The probe this page's recipe names (`R21NpecompatRows -Dr21.rows=order`) was lost with `C:\craton\jr21-ssh`;
> rebuilt as `tools/probes/jit/j27/J27W8NpeOrderRows.java` (rows `ao-mixed-plain`, `-runtime-only`,
> `-lambdas`, `-bridge`: archived names mixed with run-time names, lambda bodies, a bridge). Its header holds
> the exact fifteen-run HotSpot recipe (default CDS, `-Xshare:off`, and `-XX:ArchiveRelocationMode=0` --
> JDK 20+ already relocates by default, so the third arm is the NON-relocated mapping) and the decision rule
> of wave 5 below, plus a CratonVM self-check (three runs, identical md5). No CratonVM code change: the
> decision is HotSpot's.
>
> **j27 wave 5 (lane REFLECT, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION (HotSpot-only measurement).**
> Re-read; no code change and no probe added, on purpose. CratonVM's order for these classes IS
> deterministic (class-file order with bridge adjacency, the same in both modes), so the parity question is
> only whether HotSpot's own order is: a CratonVM rule fitted to one HotSpot run would move `--jdk-only` and
> `--compatible` off a stable answer onto one that may not reproduce on HotSpot itself. The one decider is
> still wave 1's HotSpot-only recipe below: `R21NpecompatRows -Dr21.rows=order` five times each under default
> CDS, `-Xshare:off` and `-XX:ArchiveRelocationMode=1`. If any arm differs between runs, retire this page as
> final (external gap). If all fifteen runs agree AND equal "archived names first, run-time names by first
> appearance in the constant pool", port the constructor path's approximation to `getDeclaredMethods0` as a
> parity fix (Spring batteries first: they relied on bridge adjacency).

> **j27 wave 1 (lane JNINAT, 2026-10-28): NOT FIXED -- unaddressable from the class file; a known gap, no code
> change.** What decides HotSpot's order for these classes is the address of each method-name `Symbol`
> (`Method::sort_methods` quicksorts `InstanceKlass::methods()` by `Symbol*` address, an unstable sort, at
> class-parse time). For a name the CDS archive holds the address is fixed relative to the archive; for every other
> name (most application methods, every `lambda$...$N`, every synthetic accessor) the `Symbol` is allocated at
> parse time from the symbol arena (`SymbolTable::new_symbol` -> the per-thread / global arena chunks), so its
> address -- and its rank against archived names and against other run-time names -- is a function of the
> allocation history of the HotSpot process (which classes were parsed earlier, by which thread, arena chunk
> boundaries, ASLR of the arena vs the mapped archive). None of that exists in CratonVM, and none of it is in the
> class bytes, so no deterministic rule can reproduce it; at best a rule can match one HotSpot run on one host.
> Option 1 below (measure stability) remains the only way to learn whether a useful approximation exists, and it
> needs HotSpot runs, not CratonVM code: run `R21NpecompatRows -Dr21.rows=order` (`ao-mixed-*`) on HotSpot 25 five
> times each with default CDS, `-Xshare:off` and `-XX:ArchiveRelocationMode=1`; if any arm differs between runs the
> order is not a HotSpot property and this page is final. The `Class.getDeclaredMethods()` javadoc states the
> order is unspecified; code depending on it is outside both modes' parity target. `--jdk-only` and
> `--compatible` behave the same here (class-file order with bridge adjacency, unchanged). Page kept in
> `docs/known-issues/jit/` as an external gap (AGENTS.md: "known gaps that we can do nothing about").

# `getDeclaredMethods()` order of a class with run-time method names is not HotSpot's

Status: OPEN

Area: `native-builtins/src/lang_class.rs` (`getDeclaredMethods0`), both modes.
Found by: JIT round 21 lane npecompat (AO-1).

## Evidence

HotSpot lists `InstanceKlass::methods()`, quicksorted (unstable) by name-`Symbol` address
(`jr21-npecompat-plain-class-declared-method-order-FIXED-20261027.md`). For a name that is not in the JDK's CDS
archive the `Symbol` is created at class-parse time with a malloc'd address. Its position relative to
the archive (mapped or relocated as a whole, `ArchiveRelocationMode`) and to the other run-time names
depends on the allocator (thread arena, earlier allocations) -- nothing in the class file determines it.
Most application method names, every `lambda$...$N` body and every synthetic accessor are run-time names.

CratonVM keeps class-file order with bridge adjacency for such classes (unchanged by round 21). The
constructor path (`native_class_get_declared_constructors`) instead ranks run-time names by first
appearance after all archived names -- an approximation the i4 L3 page names as such.

Probe: `C:\craton\jr21-ssh\probes\R21NpecompatRows.java -Dr21.rows=order`, rows `ao-mixed-*`, vs HotSpot 25.

## What a fix looks like

There is no exact fix from the class file alone. Options, in order of value:
1. Measure: run the `ao-mixed-*` rows on HotSpot several times (and with `-Xshare:off`, and with the
   archive relocated, `-XX:ArchiveRelocationMode=1`) to see whether HotSpot's own order is stable for
   run-time names on one host. If it is stable and equals "archived first, run-time names by first
   appearance in the constant pool", the constructor path's approximation could be applied to methods
   too (behind a default-off switch, Spring batteries first: Spring's bridge handling relied on bridge
   adjacency).
2. Otherwise this is a known gap (move to `docs/known-issues/`): frameworks must not depend on the order
   (the `Class.getDeclaredMethods()` javadoc says it is unspecified).
