# Round 14 lane compat: proposals

Ranked. Each is a direction found while working the `--compatible` collection and exception
pages in round 14 wave 1; none is implemented.

## COMPAT14-1. Retire the `--compatible` shadow rows of `Throwable.getMessage` / `toString` on the 63 exception classes

* **What.** `register_exception_extras_natives` (`native-builtins/src/lib.rs`) puts
  `getMessage` / `toString` natives on 63 JDK exception classes. Under `--compatible` the real
  `java.lang.Throwable` bytecode is loaded, so each row only shadows a correct body; strict mode
  already retired all 63 (`RETIRED_SHADOW_*` tables). Every wrong answer found on these rows
  (PatternSyntaxException / InvalidClassException `getMessage`, this wave's
  `getLocalizedMessage`, the interned result text) came from the shadow, not from a missing
  body. Let the retired triples yield to bytecode in `--compatible` too, behind a default-off
  switch first, then flip.
* **Benefit.** Removes a whole family of silent wrong answers and three diverging copies of
  `Throwable.toString` (`native_exception_to_string`, `native_throwable_to_string`,
  `throwable_to_string_text`).
* **Cost / risk.** Small code (the strict-mode retirement mechanism exists); the risk is
  boot-path throwables built before `Throwable`'s bytecode is usable -- the reason the rows
  exist. Measure with the `--compatible` app censuses (Spring Boot, WildFly, Tomcat).
* **First step.** Arm the retirement per mode in the resolver that consults
  `RETIRED_SHADOW_VME_MESSAGE_PAIR_TRIPLES`, gated by a new
  `CRATONVM_COMPAT_RETIRE_EXCEPTION_SHADOWS` (default off), and run the app censuses.

## COMPAT14-2. Live `LinkedHashSet.reversed()` by running the JDK view

* **What.** `native_lhs_reversed` builds a one-time snapshot (the open item of
  `r13w8-hashcompat3-compatible-collection-residuals-20260928.md`). The JDK 25 body is a small
  inner class (`ReverseLinkedHashSetView`) whose methods call `LinkedHashSet.this` operations and
  `map().sequencedKeySet().reversed().iterator()`. Those reach `LinkedHashMap`'s
  `LinkedKeySet(reversed = true)` / `LinkedKeyIterator`, which walk the real `tail` / `before`
  fields the natives already mirror on every write. Unregistering the one native under
  `--compatible` may be all a live view needs.
* **Benefit.** Closes the last observable item of that page for sets, without the view carrier
  of proposal C5-1.
* **Cost / risk.** Small; the risk is a native registered on the path (`sequencedKeySet`,
  `keySet`, the iterator) that answers a snapshot. Audit those triples with
  `--dump-native-registry` first.
* **First step.** A default-off `CRATONVM_COMPAT_LHS_REVERSED_BYTECODE` that skips the
  registration, and the `R13Compat5Views` / `R13Hashcompat3Semantics` probes in both states.

## COMPAT14-3. A kill-switch spelling gate

* **What.** This wave found five `native-collections` kill switches that treated only `=0` as
  off (`compat_switch_on` and four hand-rolled copies; fixed), and there are more of the shape
  in the same file (`CRATONVM_ITR_BYTECODE` accepts `0`/`false` only; three
  `v != "0" && !v.eq_ignore_ascii_case("false")` test helpers) and, by grep, in other crates. A
  ratchet in the style of `r13_misc10_create_string_ratchet.rs` counting `Some("0")` /
  `!= "0"` comparisons on `runtime_var*` results per crate would stop new ones.
* **Benefit.** A kill switch that ignores `=false` makes a bisect report "no effect" for a fix
  that never turned off (`types/src/flags.rs` `runtime_flag_default_on` documents the same
  defect for `CRATONVM_JIT_LICM`).
* **Cost / risk.** Small, test-only.
* **First step.** `rg -n 'Some\("0"\)|!= "0"' --glob '*.rs'` per crate for the baseline.

## COMPAT14-4. Keep `threshold` for every native `LinkedHashMap`, not only a custom factor

* **What.** `lhm_resize` now writes the JDK threshold only when the factor is not 0.75; a
  default map's real `threshold` field stays at its pending-size encoding (0 or
  `tableSizeFor(n)`) forever, where HotSpot holds `0.75 * capacity`. Only reflection and
  serialization-adjacent bytecode read it, but `HashMap.clone()` / `readObject` bytecode that
  runs over a native map sees a threshold the natives never meant.
* **Benefit.** One source of truth for the table's growth point; the six `(cap * 3) / 4` sites
  could then read the field.
* **Cost / risk.** Small code, but it changes the default mode's field contents (the reason it
  was not done this wave): needs its own switch and the `lhm_pending_table_size` rule rewritten
  to key on `table == null` only.
* **First step.** Write the field in `lhm_resize` unconditionally behind
  `CRATONVM_COMPAT_LHM_THRESHOLD_FIELD`, and check `lhm_pending_table_size` is never consulted
  with a table present.
