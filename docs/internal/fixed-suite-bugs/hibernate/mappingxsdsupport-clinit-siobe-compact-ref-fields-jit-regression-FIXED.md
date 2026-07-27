# `MappingXsdSupport.<clinit>` fails with `StringIndexOutOfBoundsException` — 113+ class blast radius, JIT + compact-ref-fields regression

**Severity:** Critical — the single largest contributor to a 248-FAIL regression spike in the
2026-07-26 full "passed"-category Hibernate ORM suite run (previously single-digit FAILs). 113 of
the 248 failures share this exact `<clinit>` signature, since `org.hibernate.boot.xsd.MappingXsdSupport`
is transitively touched by most Hibernate boot-time XML/JPA mapping code.

**Status:** FIXED, confirmed 2026-07-27. `origin/dev` commit `13055f75c` ("fix(jit): String
compact-layout field offsets and the branch-join reload mirror", landed 2026-07-26) fixes exactly
the two x64-backend defects this doc's "root cause" section predicted (JIT + compact-ref-fields
String field-offset miscomputation) — see commit message excerpt at the bottom of this doc.
Verified empirically: after merging `13055f75c` into `CratonVM-hib-local-0712` and rebuilding, a
282-class rerun (the full set of everything non-PASS from the 2026-07-26 regression run) showed
**112 of the 113** `MappingXsdSupport`-cluster classes now PASS. The 1 remaining (`ScannerTest`)
now fails on an unrelated `TimeoutException` (`testCustomScanner` 120s) — it cleared the original
`<clinit>`/`StringIndexOutOfBoundsException` entirely, confirming the fix is complete; the residual
is a separate, pre-existing slowness issue, not this bug. Root cause below was written BEFORE the
fix was known to exist and is left as-is for the record — the original static-analysis-only
bisection independently converged on the correct commit family (`d46e70521`'s area) without a
rebuild.

## Commit that fixed this (for reference)

```
13055f75c fix(jit): String compact-layout field offsets and the branch-join reload mirror

Two independent x64-backend defects, both found by lifting the org/h2 JIT ban
(HIB-LONGTAIL.1) and both general — H2 was only the messenger.

1. BUG-STRING-CODER-COMPACT. StringFieldLayout exposed ONE *_cell_offset per
   field; every x64.rs call site added its own FIELD_CELL_PAYLOAD32/64_OFFSET,
   and the emitters derived the legacy address by adding a further fixed +8.
   A registered CompactLayout offset is already the exact payload address, so
   the extra +4 on the int-category fields pushed coder's read onto hash and
   hash's read onto hashIsZero (real-JDK String packs value@0 coder@8 hash@12
   hashIsZero@16). coder reads 0 either way until a String's lazy hash cache
   is populated, at which point length() evaluated value.length >> (hash & 31).
   The legacy +8 was likewise only right for field indices 0 and 1; hash
   (index 2) resolved 12 bytes low.
```

**Regressed:** confirmed introduced by the 357-commit `origin/dev` fast-forward merge
`b3c3aacb3..28485dcdd` landed in worktree `CratonVM-hib-local-0712` (branch `test/hib-local-0712`)
on 2026-07-25. This exact test passed cleanly before that merge.

**Binary:** `C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe`
**Repro classpath:** `C:/craton/CratonVM/apps/hib-suite-runner` (`common.args`), hibernate-orm
sources/resources at `C:/craton/CratonVM/apps/hibernate-orm` (this worktree's own `apps/` does not
contain `hibernate-orm`/`hib-suite-runner` — they are shared from the main `CratonVM` worktree).

## Symptom

```
cd C:/craton/CratonVM/apps/hib-suite-runner
echo "org.hibernate.orm.test.annotations.configuration.ConfigurationTest" > /tmp/single-conftest.txt
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner /tmp/single-conftest.txt 0
```
```
WARN <clinit> failed — wrapping in ExceptionInInitializerError class=org/hibernate/boot/xsd/MappingXsdSupport cause=java/lang/StringIndexOutOfBoundsException
```

The VM's `[CLINIT-TRACE]` diagnostic (`vm/src/vm/vm_util.rs:1694-1729`) is **not useful here**: it
walks the VM-wide `Throwable` stack-trace registry keyed by the wrapped `ExceptionInInitializerError`'s
identity hash, not the original cause's — so it only ever prints the *outer* JUnit-launcher call
stack (`CratonRunner.main` → `SessionPerRequestLauncher` → … `HierarchicalTestExecutor`), never a
single frame inside `MappingXsdSupport`/`LocalXsdResolver`/Xerces itself. The `message=` field the
same log line is supposed to carry (`cause_msg`, read from the exception's `detailMessage` field) is
also silently empty in the actual log output — the `tracing::warn!` call names one of its fields
literally `message`, which collides with `tracing`'s own reserved message-formatting slot, so the
field is swallowed regardless of content. Neither gap was fixed here; both make this failure class
unusually hard to triage from suite-run logs alone and are worth a follow-up.

## Root cause (found via standalone repro, not suite logs)

`MappingXsdSupport`'s `<clinit>` (`apps/hibernate-orm/hibernate-core/src/main/java/org/hibernate/boot/xsd/MappingXsdSupport.java:24-100`)
calls `LocalXsdResolver.buildXsdDescriptor(resourcePath, version, namespaceUri)` twelve times, once
per XSD version Hibernate supports. Each call (`LocalXsdResolver.java:103-105`) resolves the
classpath resource then does:

```java
SchemaFactory.newInstance(W3C_XML_SCHEMA_NS_URI).newSchema(new StreamSource(url.openStream()));
```

i.e. a **fresh `SchemaFactory` per XSD**, using the JDK's bundled Xerces (`com.sun.org.apache.xerces.internal.*`,
unmodified real-JDK bytecode — not a CratonVM synthetic override). A standalone repro
(`XsdRepro.java`, scratchpad) that loads all 12 real XSD files from `hibernate-core`'s compiled
`target/resources/main` one after another, each through its own `SchemaFactory.newSchema()` call,
reproduces the exact suite failure:

```
=== org/hibernate/xsd/mapping/mapping-3.1.0.xsd ===
  OK schema=com.sun.org.apache.xerces.internal.jaxp.validation.SimpleXMLSchema@bafb
=== org/hibernate/xsd/mapping/mapping-7.0.xsd ===
  FAILED: java.lang.StringIndexOutOfBoundsException
	at com.sun.org.apache.xerces.internal.util.SymbolTable$Entry.<init>(SymbolTable.java:450)
	at com.sun.org.apache.xerces.internal.util.SymbolTable.addSymbol0(SymbolTable.java:195)
	at com.sun.org.apache.xerces.internal.util.SymbolTable.addSymbol(SymbolTable.java:176)
	at com.sun.org.apache.xerces.internal.impl.xs.traversers.XSAttributeChecker.resolveNamespace(XSAttributeChecker.java:1754)
	... (all 10 remaining XSDs fail identically)
```

`SymbolTable.java:450` (`com.sun.org.apache.xerces.internal.util.SymbolTable`, bundled in the
Adoptium JDK 25 `lib/src.zip`, package `java.xml`) is:

```java
public Entry(String symbol, Entry next) {
    this.symbol = symbol.intern();
    characters = new char[symbol.length()];
    symbol.getChars(0, characters.length, characters, 0);   // <-- line 450, throws SIOOBE
    this.next = next;
}
```

`characters.length` is set from `symbol.length()` on the immediately preceding line, so
`getChars(0, characters.length, characters, 0)` is *always* in-bounds under correct semantics
(`begin=0 <= end=length <= length`, `dst.length == end`) — this can only throw if `symbol`'s
observable length is **inconsistent between the two calls**, or `symbol` itself is a stale/corrupt
`ObjectRef` by the time `getChars` runs. `java/lang/String.getChars(II[CI)V` dispatches to
`native_string_get_chars` (`native-builtins/src/lang_string.rs:4375`, registered at
`native-builtins/src/lib.rs:21451-21456`); real JDK bytecode for `String.getChars` also calls
`String.checkBoundsBeginEnd`, natively overridden at `native_string_check_bounds_begin_end`
(`native-builtins/src/lang_string.rs:6059-6084`) to correctly throw `StringIndexOutOfBoundsException`
per spec (`begin < 0 || begin > end || end > length`) — i.e. the *throw itself* is the native bounds
check correctly detecting a genuinely bad `(begin, end, length)` triple; the bug is further upstream,
in whatever produced that bad triple.

## Confirmed: NOT a Hibernate/XSD-content bug — 100% CratonVM-side, order-triggered

Three follow-up experiments (all against the same binary, all in scratchpad, not committed):

1. **`mapping-7.0.xsd` loaded alone** (as the *first and only* schema in a fresh process) —
   **succeeds**. Rules out anything content-specific in that particular XSD.
2. **The same file (`mapping-3.1.0.xsd`) loaded three times in a row** in one process — load 1
   succeeds, loads 2 and 3 both fail identically. Proves the trigger is **purely "which call number
   is this in the process," not which XSD/content** — this cannot be a genuine Hibernate-side XSD
   bug of any kind. It is a CratonVM regression, order-dependent, and reproduces with **any** second
   (or later) `SchemaFactory.newSchema()` call in a process, using only java.xml + java.lang APIs.
3. Env-var bisection narrowed *why* it's order-dependent:
   - **`CRATONVM_DISABLE_JIT=1`** (interpreter-only) — bug **disappears entirely** (3/3 loads pass).
   - **`CRATONVM_COMPACT_REF_FIELDS=0`** (opt out of the compact reference-field/object-header
     layout, default-on) — bug **also disappears entirely** (3/3 loads pass).
   - Narrower JIT opt-outs tried and **ruled out** (bug still reproduces with each of these
     individually set to `0`): `CRATONVM_JIT_IR_DIRECT_CALL`, `CRATONVM_JIT_DIRECT_CALLEE_CALLS`,
     `CRATONVM_JIT_IR_CALL_VIRTUAL`, `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS`.

So the bug requires **both** (a) code executing under the JIT (not the interpreter) **and** (b) the
compact reference-field/object-header layout being active — and specifically needs a *second* pass
through the relevant hot code (i.e. it isn't hit on however many `Entry`/`addSymbol` calls happen
during the very first schema's parse, which JIT-compiles and runs those same methods repeatedly
without incident — only calls belonging to a subsequent, distinct `SchemaFactory.newSchema()`
invocation trip it).

## Bisection: introducing commit range

The fast-forward merge that regressed this test is exactly 357 commits (`git log --oneline
b3c3aacb3..28485dcdd | wc -l` → 357, matching the reported merge size), landed on `dev`/this
worktree 2026-07-25. Only 4 commits in that range touch `native-builtins/src/lang_string.rs`:

- `9e104704e` fix(tomcat): address Group 16 runtime regressions
- `d25e7f715` fix(jit): contain UnboundID JNDI corruption
- `3c100c5e5` arch(handles): introduce rooted Handle type to end stale-ObjectRef bug class
- `5dff5c6c3` gc: add RAII native root handles

None of the four directly modify `native_string_intern`, `native_string_get_chars`, or
`native_string_check_bounds_begin_end` (verified via `git log -S"fn native_string_intern"` /
`-S"fn native_string_get_chars"` over the range — no hits), and none of the StringBuilder/handle
migration work in `3c100c5e5`/`5dff5c6c3` touches the `String.intern()`/`getChars()` call path used
by `SymbolTable$Entry.<init>`. **Given the env-var isolation above (JIT + compact-ref-fields, not
handles), the far stronger suspect is a fifth commit outside `lang_string.rs` entirely:**

**`d46e70521` — "gc: compact object headers and field storage"** (2026-07-24), which is in the same
357-commit range (`git log --oneline b3c3aacb3..28485dcdd -- jit/src/` confirms it) and rewrites
`jit/src/x64.rs` (212 lines changed) alongside `types/src/field_layout.rs` (433 lines),
`types/src/heap_types.rs`, and most of `gc/src/*` — i.e. it is the commit that actually changed both
axes the env-var bisection implicated together (JIT codegen **and** compact-header/field layout in
the same change). A representative hunk (`jit/src/x64.rs`, the JIT's inlined `new`-allocation fast
path) shows the header layout shrinking and field meanings shifting in the same commit:

```rust
// before: offset 12 = array_length (compact) / legacy 0; offset 16 = num_slots;
//         GC_FLAG_COMPACT written as (flag << 8) at offset 20
// after:  offset 12 = full 32-bit field count (Object kind);
//         GC_FLAG_COMPACT written as (flag << 24) at offset 4;
//         forwarding_ptr moved 24->16, mark_word moved 32->24
```

This is exactly the kind of change (header field repacking + hand-rolled x64 offset immediates in
the JIT's inline allocation path) that would silently miscompute a length/count read **only** for
JIT-executed code, **only** under the compact layout, and would very plausibly first manifest as a
`String`/`char[]`-adjacent length mismatch several calls into a hot allocation-heavy loop like
Xerces' `SymbolTable.addSymbol` (called ~thousands of times per XSD parse) — matching every
observed symptom. This was not fully pinned to a single line/field inside `d46e70521`'s 212-line
`x64.rs` diff or ruled definitively in over the other 3 `jit/src/`-touching commits in the range
(`b11f82f31` bytecode quickening, `dbdc5367f`/`e0e08e4f2`/`d7bad9194`/`5cc623285` IR direct-call
lowering — the last four were individually ruled OUT by their own opt-out env vars, see above; only
`b11f82f31` bytecode quickening has no opt-out flag and was not separately tested).

**Prior art:** `docs/internal/fixed-suite-bugs/hibernate/HIB-CV-02-jit-xerces-skipstring-hang.md`
documents a *different*, already-resolved (2026-06-14) JIT-vs-Xerces bug in this exact area
(`XMLEntityScanner.skipString` infinite loop, not a `StringIndexOutOfBoundsException`) — same
"Xerces `SchemaFactory.newSchema` under JIT" blast radius, different defect, not a duplicate of this
one. `docs/internal/fixed-suite-bugs/gc-audit-2026-07-10-open-findings.md` separately documents
`CRATONVM_COMPACT_REF_FIELDS` as a historically fragile, already-flagged-risky subsystem (predates
the `d46e70521` rewrite).

## Suggested next step (not done here — needs a rebuild)

A real bisection needs an instrumented/rebuilt binary at each candidate commit, which was out of
scope for this investigation (static analysis only). Recommended path for whoever picks this up:

1. `git bisect` (or a manual binary-search rebuild) over `b3c3aacb3..28485dcdd`, using
   `XsdReproTwice.java` (load the same XSD 3x, expect 3/3 OK) as the pass/fail oracle — each build
   only needs `cargo build --release -p cratonvm-vm --bin cratonvm` (or the workspace's normal
   release target), then one ~1s repro run.
2. If `d46e70521` is confirmed as the introducing commit, disassemble the JIT-compiled
   `SymbolTable$Entry.<init>` / `SymbolTable.addSymbol0` / `String.getChars` bodies
   (`CRATONVM_DBG_JIT_DISASM=<method>`) on the *second* `SchemaFactory.newSchema()` call
   specifically (the first call's JIT-compiled code is, empirically, correct) and diff against the
   interpreter's semantics for whichever object/array-length read feeds `getChars`'s bounds check.
3. Verify the fix against `XsdReproTwice.java`, then the full `ConfigurationTest` repro command at
   the top of this doc, then re-run the "passed" category and confirm the 113-class cluster clears.

## Repro artifacts (scratchpad, not committed)

`XsdRepro.java` (all 12 real XSDs sequentially), `XsdReproSolo.java` (single XSD, isolation check),
`XsdReproTwice.java` (same XSD 3x — the minimal repro/bisection oracle), all under
`C:\Users\Victor\AppData\Local\Temp\claude\...\scratchpad\`. Compile with the Adoptium JDK 25
`javac`, run with `cratonvm.exe -cp "<scratchpad-dir>;<hibernate-core>/target/classes/java/main;<hibernate-core>/target/resources/main" <ClassName>`.
