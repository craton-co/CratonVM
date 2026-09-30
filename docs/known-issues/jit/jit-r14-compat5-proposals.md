# Round 14 wave 5, lane compat5: proposals

Ranked. Each is an idea, not a work item, until the owner queues it.

## CP5-1. A retirement-screen report per candidate class

**What.** Every census retirement so far (beans, throwable pair, `StringBuilder`, `OutputStream`,
`java.sql` this wave) ran the same four-criterion screen by hand: pure-Java `Code` on the images,
unconstructed-carrier mint sites (`try_alloc_concurrent_synthetic(.., "<class>", n)` outside
`#[cfg(test)]`), VM invariants (name arms in `vm/`, `native_override.rs` force lists, direct calls
of the native), and the registrar's category / `real_jdk()` branch. Add a `native-builtins` test
binary (not a gate) that, given `CRATONVM_RETIRE_SCREEN=<class>`, prints those four facts from the
source tree and the boot registry (category, ordinal count, `real_jdk()`-gated or not) plus the
ancestor-gate image rows the class would need. **Benefit.** A retirement candidate is screened in
one run instead of an hour of greps; the census page's "not taken" table becomes reproducible.
**Cost.** Small-medium. **Risk.** None (read-only). **First step.** Lift the mint-site regex from
`unconstructed_carrier_gate.rs` into a shared helper.

## CP5-2. Delete the dead `Bridge` rows no image declares, and gate them

**What.** Three live `Bridge` registrations name a method no supported image declares, so no door
can dispatch them and each inflates the kind map's `bridge` count:
`java/util/logging/Logger.log(Level,Supplier,Throwable)V` (the real overload takes the `Throwable`
second; `logmanager.rs` ~8639), `ConcurrentHashMap.reduceEntries(J,BiFunction)Ljava/lang/Object;`
(the real erasure returns `Map$Entry`; `native-collections` `chm_reduce_obj!`), and
`java/lang/management/MemoryUsage.<init>()V` (no no-arg constructor exists). Delete them at their
registrars (after checking no VM code calls them by that descriptor), drop the two tests that hold
them as "not retirable" (`retired_shadow.rs` `the_phase3_wave_excludes_the_registration_with_no_image_target`
and the `Logger` HELD BACK assertion) in the same commit, and add a method-level twin of
`no_image_receiver` to the stub ratchet: a `Bridge` whose `(class, name, descriptor)` resolves in
no image's hierarchy fails. **Benefit.** Honest census; SH4-2's remainder. **Cost.** Small.
**Risk.** Low (a row no image declares is unreachable by definition, except through a VM-internal
direct call, which the check excludes). **First step.** `--dump-native-registry` on 25/linux,
filter `image_declaring_method.declared == false && kind == bridge`.

## CP5-3. The values-view by-value fallback should honour a reversed carrier

**What.** C4-1 made `values_view_remove_jdk`, the iterator's node lookup and the node `removeIf`
walk a reversed `LinkedValues` tail first. The by-value fallback (`propagate_list_removal` ->
`remove_source_entry_by_value`) still removes the FIRST equal value in list order; it is reached
for a `LinkedHashMap` only with `CRATONVM_COMPAT_VALUES_REMOVE_JDK=0` /
`CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE=0`. Give `remove_source_entry_by_value` a `reversed` flag
(walk the collected pairs backwards) and pass `values_view_reversed(list)` from its two view
callers. **Benefit.** The kill-switch arms stay HotSpot-shaped for duplicated values.
**Cost.** Small. **Risk.** Low. **First step.** `R14Compat5ReversedValues` code 15 under
`CRATONVM_COMPAT_VALUES_REMOVE_JDK=0`.

## CP5-4. `java/util/logging/Formatter.formatMessage`: a measured retirement

**What.** The last live `Bridge` on `java/util/logging/Formatter` is a full transcription of the
JDK body (`jul_formatter_format_message`, `phases_early.rs`). No HotSpot difference is known, so
the only reason to retire it is AGENTS.md's "real class bytes are authoritative" -- which is worth
a measurement, not a reading: it sits on every JUL-formatted log line (Spring Boot's
`JavaLoggingSystem`). Retire it on a trial binary behind `CRATONVM_UNRETIRE_NATIVE_SHADOW`, run the
Spring Boot sample and a JUL throughput micro-bench both ways. **Benefit.** One fewer
reimplementation of a JDK body. **Cost.** One trial binary. **Risk.** Medium (log throughput,
`MessageFormat` bytecode on the hot path). **First step.** A JUL `SimpleFormatter` bench with
`{0}` parameters, 1e6 records, both arms.

## CP5-5. The `java.sql` natives in `--compatible` could go too

**What.** This wave retired the `java.sql` date/time family under `--jdk-only` only. Under
`--compatible` on a real JDK the natives still serve, over `java.util.Date` natives that write
`fastTime` directly, so the pair is self-consistent there; but they remain a second implementation
of the JDK's formatting (`Timestamp.toString` of a year above 9999, the `toLocalDateTime` mint).
Mirror the `beans_change_support_left_to_bytecode` shape (`beans_jndi.rs`): skip
`register_sql_datetime_natives` when `registry.real_jdk() && !is_jdk_only()` behind a
`runtime_flag_default_on` switch, after checking the `java.util.Date` deprecated natives in
`--compatible` leave a real-bytecode `Timestamp` consistent. **Benefit.** One implementation in both
modes. **Cost.** Small. **Risk.** Low-medium (`--compatible` must stay byte-for-byte unless it is a
correctness fix: this one is only if a divergence is measured first). **First step.** Run
`R14Compat5SqlDatetime` under `--compatible` and read which codes differ.
