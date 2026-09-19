# G22-1 — the slot collision and the matcher with no method

**Status:** FIXED-CODE / **AFTER NOT MEASURED ON A VECTOR.** Every "before" in
this record is MEASURED on a real binary, and so is every *surrogate* "after"
in §§4 and 6 — but this lane's brief forbids `cargo build`, `cargo check` and
`cargo test`, and the binary it was given (`d2e127930`,
`C:/craton/target-fcheck/release/cratonvm.exe`, mtime 2026-08-17T03:44Z) was
never rebuilt under it. **No line of this lane's Rust has been compiled.** §7
says so again, in the plainest terms available, because this directory's
standing rule is that a prediction must never be dressed as a result.

**Two binaries, and neither carries this lane's code.** The lane began on
`d2e127930` (mtime 03:44Z) and a second binary appeared under it at **04:41Z**
— built from a snapshot that predates this lane's edits. Both were used, and
both agree on every "before" here, which is why the numbers are reported once.
The 04:41Z binary is the first to carry **G13-1's interpreter change (a)**, and
it does what G13-1 §9 predicted: `RJdkMapViews`' failure text moved from

```text
AbstractMethodError: method java/util/Map.isEmpty()Z has no Code attribute
```

to

```text
IncompatibleClassChangeError: array receiver does not implement the requested
interface java/util/Map (dispatching java/util/Map.isEmpty()Z)
```

at the same frame, `RJdkMapViews.java:88`. **That confirms §1.2's chain end to
end** — the receiver really is an array — and it confirms G13-1's own
"if it still reads `AbstractMethodError`, §4.3 has a link this lane got wrong"
check in the passing direction. It closes nothing: `G22Slot` on the 04:41Z
binary still reports `LinkedHashMap$LinkedValues.this$0 -> Object[len=11]`, and
`RCrypto` still dies on `AbstractMethodError: java/nio/file/PathMatcher.matches`.

**Provenance.** Oracle: HotSpot 25.0.3+9-LTS at
`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`. Vectors from
`C:/craton/cvm-mergecheck/regression-suite/build`. Probes written for this
lane, all in `scratchpad/g22/`:

| probe | what it measures |
|---|---|
| `G22Slot.java` | the six view carriers' declared fields, by reflection, on both VMs |
| `G22Fam.java` | 20 Collection operations x 5 map families x `values`/`keySet` |
| `G22Proxy.java` | `RJdkMapViews` sections 1a+1b **verbatim**, retargeted at `HashMap` / `TreeMap` / `ConcurrentHashMap` |
| `G22List.java` | `RJdkMapViews` sections 2a-2c **verbatim** |
| `GlobsRef.java` | the JDK's own `sun.nio.fs.Globs.toWindowsRegexPattern`, by reflection, over 47 globs |
| `G22Match.java` | `getPathMatcher(...).matches(p)` vs `Pattern.compile(Globs(g), 66).matcher(p.toString()).matches()`, 58 rows, both VMs |
| `G22Exc.java` | `PatternSyntaxException(desc, regex, index).getMessage()` on both VMs |

This lane owns exactly `native-collections/src/lib.rs` and
`native-builtins/src/phases_late/nio_file.rs`. It is G13-1's N1, N2 and N4.

---

## 0. The headline

| | before (MEASURED) | after (see the column header) |
|---|---|---|
| `LinkedHashMap$LinkedValues.this$0` | `[Ljava.lang.Object;[len=11]` | UNMEASURED |
| `HashMap$Values.this$0` / `TreeMap$Values.this$0` / `CHM$ValuesView.map` | `null` (HotSpot: the map) | UNMEASURED |
| `LinkedHashMap.values()` family | **7 of 7 rows diverge** | UNMEASURED |
| `RJdkMapViews` | exit 1, 0 of 6 `CK` lines, dies at line 88 | UNMEASURED |
| — the error at that line | `AbstractMethodError … Map.isEmpty()Z` on `d2e127930`; **`IncompatibleClassChangeError: array receiver …`** on the 04:41Z binary | UNMEASURED |
| — its 46 LinkedList checks, run verbatim as `G22List` | **PASS, 46 checks, diff empty** | (already green) |
| — its 28 values checks, run verbatim against the three one-field carriers | **PASS, 84 checks (28 x 3), diff empty** | (already green) |
| `PathMatcher.matches` | `AbstractMethodError`, **58 of 58** rows | UNMEASURED |
| `Pattern.compile(Globs(g), 66).matcher(path.toString()).matches()` on CratonVM | **57 of 58 rows agree with HotSpot's `PathMatcher`** | (the runtime half already works) |
| `PatternSyntaxException(desc, regex, i).getMessage()` on CratonVM | **byte-identical to HotSpot, 8 of 8** | (the message half already works) |
| `RCrypto` | exit 1, 16 of 20 `CK` lines | UNMEASURED |

Two bugs, two files, and in both cases the measurement that mattered was of the
**family the bug is not in** — the carriers that do NOT collide, and the two
JDK components the missing native was going to have to call.

---

## 1. N1 — the `LinkedValues` field-slot collision

### 1.1 The measurement, MEASURED on both VMs

`G22Slot.java`, each run with `--add-opens java.base/java.util=ALL-UNNAMED`
(and `java.util.concurrent` for the CHM row):

```text
                                        HotSpot                   CratonVM --jdk-only
HashMap$Values.this$0                   java.util.HashMap         null
LinkedHashMap$LinkedValues.reversed     java.lang.Boolean         java.lang.Boolean
LinkedHashMap$LinkedValues.this$0       java.util.LinkedHashMap   Object[len=11]
TreeMap$Values.this$0                   java.util.TreeMap         null
CHM$ValuesView.map                      java.util.concurrent.…    null
```

The `getClass()` row is correct on all five families on both VMs, including
`Hashtable.values()` answering `java.util.Collections$SynchronizedCollection`.
It is only the *contents* of the carrier's own declared fields that diverge.

`len=11` is `make_view_list_of`'s own
`cap = max(vals.len(), AL_DEFAULT_CAPACITY = 10) + 1` for a three-element view.
It is not a coincidence-shaped number; it is that buffer.

`G22Fam.java`, 5 families x 20 operations, both VMs, diffed after `tr -d '\r'`.
Of 100 rows, the divergences are:

```text
LinkedHashMap.values.size                3  ->  0
LinkedHashMap.values.isEmpty         false  ->  true
LinkedHashMap.values.toArray.len         3  ->  AbstractMethodError: java/util/Map.isEmpty()Z
LinkedHashMap.values.toArray.content [1,2,3] -> AbstractMethodError
LinkedHashMap.values.newArrayList.size   3  ->  AbstractMethodError
LinkedHashMap.values.toArrayTyped.len    3  ->  AbstractMethodError
LinkedHashMap.values.stream.count        3  ->  AbstractMethodError
LinkedHashMap.values.forloop             3  ->  AbstractMethodError
LinkedHashMap.values.contains         true  ->  AbstractMethodError
LinkedHashMap.values.iteratorNext        1  ->  AbstractMethodError
LinkedHashMap.values.live      3/4/3/[2,3,4] -> 0/0/0/[1,2,3]
```

`HashMap`, `TreeMap`, `Hashtable` and `ConcurrentHashMap` values views are
clean apart from two unrelated findings recorded in §5. Every `keySet()` and
every `entrySet()` is clean. This confirms G13-1's 7-of-7 count and extends it:
the `size`/`isEmpty` pair diverges in **both** policy modes without going near
the interpreter, and `iterator().next()` and `contains` diverge too.

### 1.2 The cause, and why it is exactly one class

`javap -p`, JDK 25.0.3+9: five of the six `MAP_VIEW_CARRIERS` declare exactly
one field, and one does not.

```text
java.util.HashMap$Values                 final HashMap this$0;        -> this$0 @0
java.util.TreeMap$Values                 final TreeMap this$0;        -> this$0 @0
java.util.TreeMap$EntrySet               final TreeMap this$0;        -> this$0 @0
java.util.Hashtable$ValueCollection      final Hashtable this$0;      -> this$0 @0
ConcurrentHashMap$ValuesView (inherited) final CHM map;               -> map    @0
java.util.LinkedHashMap$LinkedValues     final boolean reversed;      -> reversed @0
                                         final LinkedHashMap this$0;  -> this$0   @1
```

The real-JDK `ArrayList` layout puts `AbstractList.modCount` at 0,
`elementData` at **1** and `size` at **2**. The carriers kept their list state
at those ABSOLUTE indices — the `MAP_VIEW_CARRIERS` comment argued this was
safe because they sit "past the single `this$0` these classes declare", which
is true of five carriers and false of the sixth. So `al_set_data` wrote the
element buffer into slot 1, which on `LinkedValues` is `this$0`; and
`values_view_class_source` resolves `this$0` **by name** — precisely because
`LinkedValues` puts it at slot 1, as its own doc block says — and read the
buffer back as the source map.

Downstream, `vc_route` handed that `Object[]` to `collect_entries_any`, which
invoked `java/util/Map.isEmpty()Z` on an array. That is the whole of G13-1's
Mechanism B, and its `AbstractMethodError` names a class that has nothing wrong
with it.

### 1.3 The fix

`native-collections/src/lib.rs`. New `view_carrier_slots(ctx, cid)`: a
`MAP_VIEW_CARRIERS` object's list slots are `(D, D+1, D+2)` where `D` is
`class_num_total_fields(cid)` — the carrier's own declared field count,
inherited fields included (required: `ConcurrentHashMap$ValuesView` declares
nothing and inherits `map`). `al_slots_for_uncached` answers this before it
answers anything else about a carrier, under a new `AlLayout::ViewCarrier`
that reads and writes exactly like `AlLayout::ArrayList` everywhere else.

**This is the same move the file already makes for `java/util/Vector`** — a
receiver that is not an `ArrayList` does not get `ArrayList`'s slots — and it
is deliberately as narrow as the measurement:

| carrier | declared fields | slots before | slots after |
|---|---|---|---|
| `HashMap$Values` | 1 | (1, 2, 3) | **(1, 2, 3)** |
| `TreeMap$Values` | 1 | (1, 2, 3) | **(1, 2, 3)** |
| `TreeMap$EntrySet` | 1 | (1, 2, 3) | **(1, 2, 3)** |
| `Hashtable$ValueCollection` | 1 | (1, 2, 3) | **(1, 2, 3)** |
| `ConcurrentHashMap$ValuesView` | 1 | (1, 2, 3) | **(1, 2, 3)** |
| `LinkedHashMap$LinkedValues` | **2** | (1, 2, 3) | **(2, 3, 4)** |

Five of six layouts are bit-identical to what they were. Only the class the
measurement indicts moves. `alloc_view_carrier` now derives the field count
itself from the same `ClassId` rather than taking `al_slots(ctx).2` from its
four callers, so the count allocated and the indices later read can no longer
disagree — a two-field carrier allocated with three slots would have had
`al_set_size` silently skip its out-of-bounds write and report size 0.

Unresolvable (`class_num_total_fields == 0`: synthetic-JDK, or a call before
the carrier links) falls back to `al_slots_resolved`, i.e. the historical
layout, and — like the arm above it — propagates `None` rather than caching a
verdict that stops being true when `ArrayList` loads.

### 1.4 N2, and why it needed a second guard

`al_mod_count_slot` resolved `java/util/AbstractList.modCount` to slot **0**
and was permitted to write there on any receiver whose data/size slots were
elsewhere. On a carrier, slot 0 is not `modCount` at all — it is the carrier's
own first declared field: `this$0` / `map`, **references**, on five of them,
and `reversed`, a boolean, on the sixth. `al_set_size` bumps that counter on
every logical size change, so every values view minted by this VM took an
`Int` store into a declared reference slot. That is the type-punning store
`al_mod_count_slot`'s own doc block already warns about for the synthetic
layout, and it is the most likely reason `HashMap$Values.this$0` reflected as
`null`. A carrier now reports no `modCount` slot (it extends
`AbstractCollection`, not `AbstractList`; it has none). Views lose
comodification detection, the direction that function already documents as
safe.

With that slot no longer being scribbled on, `store_view_carrier_backref`
writes the real backing map into it at all four mint sites — `native_map_values`,
`make_live_values_list`, `make_view_list_of`, `native_chm_values` — which is
G13-1's N2. It refuses any slot at or above the receiver's `elementData` slot,
so it structurally cannot re-create the collision it was added alongside.

### 1.5 `vc_route` — the previous lane's "inert" claim is FALSIFIED, and this fix makes it true

A prior lane recorded that `vc_route` is inert under `--jdk-only`. **It is
not, and the corpus census proves it**: G13-1 measured
`[CANONICAL] java/util/Map java/util/HashMap isEmpty 1` in `RJdkMapViews` —
one row across 105 main classes x 2 policy modes — and that row exists only
because `vc_route` fired. It fired for exactly one receiver shape: a
`LinkedValues` whose `this$0` had been overwritten with a non-null `Object[]`.
`is_values_view_class` was satisfied for all five names all along; what gated
the route was `values_view_class_source` returning `None`, and it returned
`None` only because nothing ever populated `this$0`. The route was reachable
by accident and only on the broken path.

Populating `this$0` (§1.4) would therefore have made `vc_route` fire for
**every** carrier — and that is unbounded recursion, because `vc_route` calls
back into the same native entry point over a carrier of the same class, which
satisfies its own test again. `is_own_view_carrier` now declines any carrier
this crate minted (identified by the element buffer's trailing source-map
marker, `values_view_source`), on both `vc_route` and `vc_route_source_size`.

**After this change `vc_route` is unreachable, and by construction rather than
by accident.** Every carrier that exists carries the marker; a view-class object
that does not would have to come from real JDK bytecode running
`new HashMap$Values(...)`, which `force_native_over_real_jdk_bytecode` blocks
for exactly these class names. The guard is a no-op today — it changes nothing
on the current binary — and it is what keeps §1.4 from being a stack overflow.

### 1.6 One more measured gap in the same family: `toString()` was a dead snapshot

`native_al_to_string` was the one element-reading list native that did **not**
open with `resync_values_view`. `native_al_get`, `native_al_contains`,
`native_al_iterator`, `native_al_for_each` and `native_al_stream` all do.
MEASURED on all five families (`G22Fam`, the `.live` row): after
`m.put("d","4"); m.remove("a")` on a three-entry map, `v.size()` answered the
live **3** and `v.toString()` answered the captured `[1, 2, 3]` where HotSpot
answers `[2, 3, 4]`.

`RJdkMapViews` does not catch this, and the reason is worth writing down: its
`valuesLiveness` calls `v.contains(...)` between the mutation and the
`toString()`, and `contains` resyncs the backing array as a side effect. The
gap is real and family-wide; the vector is blind to it by one line's accident.
Fixed by the same resync every sibling native already performs.

---

## 2. N4 — `PathMatcher`, an object with zero usable methods

### 2.1 The measurement

`FileSystem.getPathMatcher(String)` was registered as

```rust
|ctx, _args| { try_alloc_concurrent_synthetic(ctx, "java/nio/file/PathMatcher", 0) }
```

— the pattern ignored, no state, and **no `PathMatcher.matches` registration
anywhere in the VM**. G13-1's ratio, at its limit: 0 of 1.

MEASURED over a 292-row census of the whole syntax (`glob:`/`regex:`, `**`,
`?`, classes, alternation, escaping, prefixes, nulls) on both VMs: **195 rows
answer `AbstractMethodError: java/nio/file/PathMatcher.matches(...)Z`**, and
**every single row where HotSpot REFUSES the pattern instead built a matcher
happily** — `getPathMatcher(null)`, `"*.txt"` with no prefix, `""`, `":"`,
`"foo:*.txt"`, `glob:[abc`, `glob:{a,b`, `glob:abc\`, `regex:[a-`, `regex:(`.
CratonVM's `getPathMatcher` performed zero parsing and never threw.

`G22Match.java`, 58 rows, MEASURED: the `getPathMatcher(...).matches(...)`
column is `AbstractMethodError` in 58 of 58.

`RCrypto` reaches it from inside java.base, which is why the vector's source
names neither class:

```text
AbstractMethodError: java/nio/file/PathMatcher.matches(Ljava/nio/file/Path;)Z
  at RCrypto.main(RCrypto.java:545)
  at javax/crypto/KeyGenerator.getInstance(KeyGenerator.java:288)
  at javax/crypto/JceSecurity.<clinit>(JceSecurity.java:111)
  at javax/crypto/JceSecurity.setupJurisdictionPolicies(JceSecurity.java:347)
  at java/nio/file/Files.newDirectoryStream(Files.java:509)
  at java/nio/file/Files$1.accept(Files.java:503)
```

`Files$1.accept` is the real JDK bytecode `matcher.matches(entry.getFileName())`.

### 2.2 The two closures that are NOT the fix

**Deleting the registration is not the fix**, even though
`sun.nio.fs.WindowsFileSystem.getPathMatcher` is a complete implementation and
CratonVM can load and run `sun.nio.fs.Globs` (proved in §2.4).
`FileSystems.getDefault()` returns CratonVM's own synthetic
`java/nio/file/FileSystem` — `p57_default_filesystem_singleton`; the
`sun.nio.fs.WindowsFileSystem` that `getClass()` reports is the
`jdk_concrete_getclass_alias` mapping, not its real class — and
`java.nio.file.FileSystem.getPathMatcher` is **abstract**. Unregistering moves
the `AbstractMethodError` one call earlier.

**Making `newDirectoryStream`'s glob overload skip the matcher is not the fix
either.** That is exactly the fabricated success `d378eee51` removed, and it is
what made `RCrypto` green over a filter that was never invoked.

### 2.3 The syntax, settled

G13-1 explicitly did not settle whether the full `sun.nio.fs.Globs` syntax was
needed. It is. The reference implementation was recovered from
`lib/src.zip` (`C:\craton\jdk25src` **does not exist** on this host) and ported
statement for statement into `p57_globs_to_regex`. The Windows source is:

```java
public PathMatcher getPathMatcher(String syntaxAndInput) {
    int pos = syntaxAndInput.indexOf(':');
    if (pos <= 0) throw new IllegalArgumentException();
    String syntax = syntaxAndInput.substring(0, pos);
    String input  = syntaxAndInput.substring(pos+1);
    String expr;
    if (syntax.equalsIgnoreCase("glob"))       expr = Globs.toWindowsRegexPattern(input);
    else if (syntax.equalsIgnoreCase("regex")) expr = input;
    else throw new UnsupportedOperationException("Syntax '" + syntax + "' not recognized");
    final Pattern pattern = Pattern.compile(expr,
        Pattern.CASE_INSENSITIVE | Pattern.UNICODE_CASE);
    return path -> pattern.matcher(path.toString()).matches();
}
```

Four load-bearing facts, each MEASURED and each easy to get wrong:
`pos <= 0` (not `< 0`), so a LEADING colon is refused; `equalsIgnoreCase` and
no trimming (`GLOB:` works, ` glob:` does not); `CASE_INSENSITIVE |
UNICODE_CASE` = **66** is applied to `regex:` patterns too, not only globs; and
`matches` runs against `path.toString()` — the whole normalized path — with
`Matcher.matches` semantics, so it is fully anchored and there is no
basename-only mode.

**The translation table (Windows branch), MEASURED against the JDK's own
`Globs.toWindowsRegexPattern` by reflection over 47 globs — every row below is
a `GlobsRef.java` output line, not a derivation:**

| glob | Windows regex | note |
|---|---|---|
| `*.txt` | `^[^\\]*\.txt$` | `*` stops at the separator |
| `a?c` | `^a[^\\]c$` | `?` is one char, never the separator |
| `**.txt` | `^.*\.txt$` | `**` crosses separators |
| `**/*.txt` | `^.*\\[^\\]*\.txt$` | |
| `src/**` | `^src\\.*$` | so bare `src` does NOT match |
| `***.txt` | `^.*[^\\]*\.txt$` | `**` then `*`, not a third wildcard |
| `sub/a.txt` | `^sub\\a\.txt$` | `/` in a PATTERN is the separator |
| `sub\a.txt` (one `\`) | `^suba\.txt$` | **a lone `\` is the ESCAPE**, not a separator |
| `sub\\a.txt` | `^sub\\a\.txt$` | two are the separator |
| `a//b` | `^a\\\\b$` | two literal separators — unmatchable, see §5 |
| `*.{java,class}` | `^[^\\]*\.(?:(?:java)\|(?:class))$` | |
| `{a,}` | `^(?:(?:a)\|(?:))$` | empty alternative is legal |
| `{}` | `^(?:(?:))$` | empty group is legal |
| `{a}{b}` | `^(?:(?:a))(?:(?:b))$` | sequential groups legal; only NESTING is banned |
| `a}b` / `a,b` | `^a}b$` / `^a,b$` | stray `}`/`,` are literals |
| `[abc].txt` | `^[[^\\]&&[abc]]\.txt$` | class excludes the separator by intersection |
| `[!a-z].txt` | `^[[^\\]&&[^a-z]]\.txt$` | `!` negates |
| `[^abc].txt` | `^[[^\\]&&[\^abc]]\.txt$` | **`^` is a LITERAL in a glob class** |
| `[a-]` / `[-a]` | `^[[^\\]&&[a-]]$` / `^[[^\\]&&[-a]]$` | hyphen literal at either end |
| `[a&&b]` | `^[[^\\]&&[a\&&b]]$` | `&&` escaped so it is not intersection |
| `[[]` | `^[[^\\]&&[\[]]$` | |
| `[]` | `^[[^\\]&&[]]$` | **not refused by `Globs`** — `Pattern.compile` refuses it |
| `[]]` | `^[[^\\]&&[]]\]$` | first `]` closes; second is a literal |
| `a+b` / `a(b)c` / `a.b` | `^a\+b$` / `^a\(b\)c$` / `^a\.b$` | `regexMetaChars` = `.^$+{[]\|()` |
| `a-b` / `a&b` | `^a-b$` / `^a&b$` | and NOTHING outside that set is escaped |
| `\a` | `^a$` | escaping a non-meta is a no-op |
| `\*.txt` | `^\*\.txt$` | |
| `` (empty) | `^$` | |

The Unix branch differs in exactly four sites: `[^/]*`, `[^/]`, `/`, `[[^/]&&[`.

**The six refusals `Globs` itself raises**, with the exact `desc` and the exact
index, MEASURED (the unbalanced apostrophes in two of them are the JDK's, not
typos):

| glob | desc | index |
|---|---|---|
| `abc\` / `\` | `No character to escape` | 3 / 0 |
| `[/]` / `[a/b]` / `[a\b]` | `Explicit 'name separator' in class` | 1 / 2 / 2 |
| `[abc` / `[` | `Missing ']` | 3 / 0 |
| `[z-a]` / `[a-c-e]` | `Invalid range` | 1 / 4 |
| `{a,{b,c}}.txt` | `Cannot nest groups` | 3 |
| `{a,b` / `{` | `Missing '}` | 3 / 0 |

Everything else is refused one layer down, by `Pattern.compile`, over the
**translated** string with an index into *that* — e.g. `glob:[]` gives
`Unclosed character class near index 12` quoting `^[[^\\]&&[]]$`. A native that
raised the `Globs` error for those would move an exception between two classes.

Prefix handling, MEASURED: `"*.txt"` / `""` / `":"` / `":glob"` all raise an
`IllegalArgumentException` whose **`getMessage()` is null**; `"foo:*.txt"`
raises `UnsupportedOperationException: Syntax 'foo' not recognized`; `glob:a:b`
is the glob `a:b` (only the first colon splits); `glob:` and `regex:` are legal
and match only the empty path. Nulls, MEASURED, are JVM-synthesized helpful
NPEs and can only be transcribed:
`Cannot invoke "String.indexOf(int)" because "syntaxAndInput" is null` and
`Cannot invoke "java.nio.file.Path.toString()" because "path" is null`.

Windows case-insensitivity is total and reaches inside classes: `[a-z].txt`
matches `A.txt`, and `[!a-z].txt` does **not**.

### 2.4 The fix, and the two halves of it that ARE measured

`getPathMatcher` now parses, translates and compiles; `PathMatcher.matches` is
registered on `java/nio/file/PathMatcher` and does
`pattern.matcher(path.toString()).matches()`. The matcher is a one-field object
stamped with the interface — the same shape the `Path.iterator` body a few
hundred lines above already uses for `java/util/Iterator`.

The Rust translation is the only new logic; the runtime is the JDK's own. Both
of the JDK components it leans on were measured on CratonVM first:

* **`Pattern.compile(expr, 66).matcher(Paths.get(in).toString()).matches()`
  IS the PathMatcher.** MEASURED on HotSpot over 58 rows: that expression and
  `getPathMatcher("glob:"+g).matches(Paths.get(in))` agree on **58 of 58**.
* **That expression already works on CratonVM.** MEASURED: CratonVM's answers
  agree with HotSpot's on **57 of 58** rows. The single divergence is §5's
  `Paths.get` bug and has nothing to do with matching.
* **The refusal messages compose correctly on CratonVM.** MEASURED
  (`G22Exc.java`): `new PatternSyntaxException(desc, regex, index).getMessage()`
  is **byte-identical** on both VMs for all eight refusals — including the
  `\r\n` line separator and the caret line, and including its omission when
  `index == pattern.length()` — and `new IllegalArgumentException().getMessage()`
  is `null` on both. The native therefore builds the real Java objects rather
  than reproducing three moving parts of a string.
* **CratonVM runs `sun.nio.fs.Globs` correctly.** MEASURED: invoked
  reflectively over the 58-row table, CratonVM's `toWindowsRegexPattern`
  output is **identical to HotSpot's on every row**. That is the oracle the
  Rust port targets, and 47 of those translations are pinned as unit tests.

---

## 3. What changed, by file

**`native-collections/src/lib.rs`**

* `AlLayout::ViewCarrier` — new variant; reads/writes as `ArrayList`, differs
  only in the slot triple it is paired with.
* `view_carrier_slots(ctx, cid)` — the triple, above the carrier's own
  declared fields.
* `al_slots_for_uncached` — answers it for every `MAP_VIEW_CARRIERS` name, in
  the arm next to the existing `java/util/Vector` one. The old
  `Some(n) if is_map_view_carrier(n) => AlLayout::ArrayList` arm is removed as
  unreachable.
* `alloc_view_carrier` — derives its own field count; the `n_fields` parameter
  and its four `al_slots(ctx).2` call sites are gone.
* `al_mod_count_slot` — `None` for a carrier (§1.4).
* `store_view_carrier_backref` — new; called at the four mint sites.
* `is_own_view_carrier` — new; guards `vc_route` and `vc_route_source_size`
  (§1.5).
* `native_al_to_string` — resyncs a live view (§1.6).
* `MockCtx` — `class_num_total_fields` + `define_total_fields`, so the layout
  is testable at all.
* Tests: `view_carrier_slots_clear_the_carriers_own_declared_fields`,
  `view_carrier_behaves_as_a_list_everywhere_but_its_slots`,
  `a_view_carrier_has_no_mod_count_slot`,
  `the_backref_lands_on_this_dollar_zero_and_nowhere_else`.

**`native-builtins/src/phases_late/nio_file.rs`**

* `p57_globs_to_regex` — the `Globs` port; `P57_GLOB_META`, `P57_REGEX_META`,
  `P57_MATCHER_FLAGS`, `P57_MATCHER_PATTERN_FIELD` and three small helpers.
  Operates on UTF-16 code units, not `char`s, so every index in an error
  message is the index `String.charAt` would have produced.
* `p57_throw_pattern_syntax`, `p57_bare_illegal_argument` — the two exception
  shapes that must be real Java objects (§2.4).
* `getPathMatcher` — rewritten.
* `java/nio/file/PathMatcher.matches` — registered, for the first time.
* Tests: `windows_translation_matches_the_jdk` (against `GlobsRef`'s measured
  output), `unix_branch_uses_the_forward_slash`,
  `the_six_refusals_carry_the_jdk_text_and_index`,
  `globs_defers_two_cases_to_the_regex_engine`,
  `matcher_flags_are_case_insensitive_plus_unicode_case`.

`rustfmt --edition 2021 --check`, run **in place, in each file's own tree**,
reports the same hunk counts as before this lane started — 89 in
`native-collections/src/lib.rs`, 52 in `nio_file.rs` — and every hunk is at a
pre-existing site. (The baseline for the 89 was taken by rustfmt-checking
`HEAD`'s copy **with `identity_hash.rs` beside it**: without the sibling module
rustfmt reports 0 diffs and exits 0, the vacuous check this directory warns
about.) Zero CR bytes in either file.

---

## 4. The surrogate "after" for `RJdkMapViews`, MEASURED

This lane could not rebuild, so it measured the closest thing that does not
require one: **`RJdkMapViews`' own assertions, verbatim, on the paths the fix
makes `LinkedValues` join.**

`RJdkMapViews` reports `checks=74`. It splits exactly:

* **46 checks** — sections 2a-2c, the `LinkedList` half. Extracted verbatim as
  `G22List.java` and run on the current binary: **PASS, 46 checks, diff against
  HotSpot empty.** The vector has never reached these; they are already green.
* **28 checks** — sections 1a (`valuesIdentitySemantics`) and 1b
  (`valuesLiveness`). Extracted verbatim as `G22Proxy.java` with the map family
  swapped, and run three times — `HashMap`, `TreeMap`, `ConcurrentHashMap`:
  **PASS, 84 checks (28 x 3), diff empty, in `--jdk-only` AND in Compatible.**

The three families that pass those 28 assertions are exactly the carriers whose
`elementData` slot does not collide with a declared field. The family that
fails them is the one whose slot does. **The 74 checks of `RJdkMapViews` are
therefore 46 already-measured-green plus 28 measured-green-on-every-
non-colliding-carrier.** That is the strongest evidence available without a
build, and it is still not a measurement of this lane's code.

`RCrypto` has no equivalent surrogate: the two components its fix composes are
measured on CratonVM (§2.4), the translation is measured against the JDK
(§2.3), but nothing has run the composition.

---

## 5. Measured, NOT fixed — three findings for whoever is next

**(a) `Paths.get` does not collapse separator runs (Windows).** MEASURED, and
it is the one row of 58 where CratonVM's regex matching disagrees with
HotSpot's `PathMatcher`:

```text
Paths.get("a//b").toString()   HotSpot: a\b        CratonVM: a\\b
glob:a/b vs input a//b         HotSpot: true       CratonVM: false
```

Two more from the same parser, MEASURED: `Paths.get("a ")` (trailing space) is
accepted where HotSpot throws
`InvalidPathException: Trailing char < > at index 1: a `, and CratonVM's
`InvalidPathException` message is `Illegal char: *.txt` where HotSpot says
`Illegal char <*> at index 0: *.txt`. All three are in
`nio_file.rs`'s `p57_alloc_path_checked` / `p57_read_path`, which IS this
lane's file — deliberately not taken, because path normalization is on every
`Files` path in the VM, the blast radius cannot be assessed without running
the corpus, and none of it is on `RCrypto`'s route.

**(b) `Hashtable.values()` iterates in the wrong order.** MEASURED
(`G22Fam`): HotSpot `[2, 1, 3]`, CratonVM `[1, 2, 3]`, for
`{a=1, b=2, c=3}` — HotSpot's is the real bucket order. `toString`, `toArray`
and `iterator().next()` all disagree. Unrelated to the carrier layout; no
corpus vector asserts it.

**(c) `TreeSet$Itr` is refused as a fabrication.** MEASURED, seen once per
`TreeMap` values-view iteration under `--jdk-only`:
`refusing to fabricate a compatibility stand-in ... class="java/util/TreeSet$Itr"
fields=4 requested_by="native-collections\src\lib.rs:47517"`. The workload
still passes; the refusal is silent to Java. In this lane's file, but a
different family, and taking it would have widened this change past what the
measurement indicts.

---

## 6. What this lane did NOT do

* **It did not build the binary, and therefore did not compile a single line
  it wrote.** `cargo build` / `cargo check` / `cargo test` are forbidden by its
  brief. A binary DID appear at 04:41Z while the lane ran, and the lane
  measured on it — but it was built from a snapshot predating these edits
  (proved, not assumed: `G22Slot` on it still reports
  `LinkedValues.this$0 -> Object[len=11]`, and `PathMatcher.matches` still
  raises `AbstractMethodError`). `rustfmt --check` parses; it does not
  type-check. **Every "after" for a vector in this record is
  UNMEASURED.** The next lane to build should re-run `RJdkMapViews` and
  `RCrypto` first, and treat §§1.3-1.6 and 2.4 as unverified until it does.
* **It did not run `regression-suite/run.sh`,** nor any state-changing git
  command.
* **It did not touch any file but its two.** `net_phase_e.rs` (G13-1's N3),
  `interpreter.rs` and `native-builtins/src/lib.rs` are other lanes'.
* **It did not register `java/util/Map.isEmpty()Z`.** G13-1's argument stands:
  in Compatible mode the substitution currently *succeeds* and reports a
  three-entry view as empty, and registering the door would restore that silent
  wrong answer and remove the only signal.
* **It did not widen the carrier change past `LinkedValues`.** Five of the six
  layouts come out bit-identical (§1.3) precisely so the four measured-clean
  map families cannot regress.
* **It did not remove `vc_route`,** though §1.5 argues it is now unreachable.
  Deleting a route on an argument rather than a measurement is how the previous
  "it is inert" claim came to be wrong.
* **It did not verify the four currently-green vectors after its change** —
  it cannot. It verified them BEFORE: `RJdkCollections` (7 lines),
  `RJdkViews` (19), `RCollections` (1) and `RChmKeySetView` (4) all diff empty
  against HotSpot on `d2e127930`.

---

## 7. The one-sentence version

One carrier out of six declares two fields instead of one, so `ArrayList`'s
absolute `elementData` slot landed on `LinkedHashMap$LinkedValues.this$0` and a
values view handed its own element buffer back as the map it was a view of;
and one interface was minted with no implementation of the only method it
declares, so `java.nio.file.PathMatcher` was an object that could not be asked
anything — two mistakes that produced the same sentence, `AbstractMethodError:
... has no Code attribute`, for the same reason G13-1 gave: it is what this
interpreter says when something upstream hands it a receiver it cannot
dispatch on.
