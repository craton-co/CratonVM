# W7-43 — `Formatter.formatMessage` never called the formatter, and the library's own swallow is why that had to be proved rather than assumed

**Status:** **FIXED**, both modes, in `native-builtins/src/phases_early.rs`.
`java.util.logging.Formatter.formatMessage(LogRecord)` returned
`record.getMessage()` verbatim; it now reproduces JDK 25's five steps.
`RJdkLogging.recordPayloads` is the strict corpus' last red (`--jdk-only`
69 passed / 1 failed) and this is its cause.

**Binary for every measurement below:** `target/release/cratonvm.exe` as found
in `C:/craton/CratonVM`, built 2026-08-12 00:21 from `dev` — i.e. **before**
this lane's change — against `Eclipse Adoptium jdk-25.0.3.9-hotspot`, with
HotSpot 25.0.3+9 as the oracle on every arm. Nothing was rebuilt in this lane;
§6 separates what was run from what is reasoned.

---

## 1. The symptom

```text
CK RJdkLogging supplier=evaluated:1 thrown=IllegalStateException
Exception in thread "main" java/lang/AssertionError: Formatter.formatMessage must substitute; got one={0} two={1}
	at RJdkLogging.recordPayloads(RJdkLogging.java:411)
```

Everything ahead of the failing line passes, and that is the important part of
the report rather than a preamble. `recordPayloads` asserts, in order, that
`log(LogRecord)` delivers the SAME object, that the record keeps the **raw**
pattern `one={0} two={1}`, and that it carries both parameters `"A"` and `"B"`.
All four hold. **HotSpot substitutes in the FORMATTER, never in the record**,
so a green record side and a red formatter side is exactly the shape of a
formatter defect — the fixture is designed to separate them and it did.

## 2. Two causes, one symptom, and why the difference is not academic

The JDK's `formatMessage` ends with a deliberate swallow:

```java
        } catch (Exception ex) {
            // Formatting failed: use localized format string.
            return format;
        }
```

So there are two entirely different defects that both print
`one={0} two={1}`:

| cause | mechanism | what a fix to the other one does |
|---|---|---|
| **A — the guard/body** | the "is it a `java.text` pattern" scan is wrong or absent, so the method returns early with the raw message | — |
| **B — the dependency** | `java.text.MessageFormat.format` is missing or stubbed, throws, and the JDK's own swallow converts the throw into "return the raw pattern" | a corrected guard reaches a throwing `MessageFormat`, the swallow eats it, and the symptom is **unchanged** — an inert fix that looks like a wrong diagnosis |

A library's own fallback presenting a real defect as "wrong formatting" is a
recurring shape in this tree, and `java.util.logging` is where it has already
cost a lane: W7-25-jul-getlogger-regression.md and
W7-35-jul-supplier-and-payload-residuals.md are both records of JUL defects
stacked behind one another, each visible only once the one above it moved.
Cause B had to be ruled out **before** the guard was touched, not after.

### How it was told apart

Directly, on the pre-fix binary, by asking `MessageFormat` the question
`formatMessage` would have asked it:

```text
MessageFormat.format("one={0} two={1}", {"A","B"})
  HotSpot 25.0.3+9      one=A two=B
  CratonVM --jdk-only   one=A two=B
  CratonVM --real-jdk   one=A two=B
```

and then, because "it substitutes" is a weaker claim than "it is the real
implementation", on four cases where a hand-rolled substituter and the real
`java.text.MessageFormat` disagree:

| pattern, args | HotSpot | CratonVM `--jdk-only` | CratonVM `--real-jdk` |
|---|---|---|---|
| `set={x} v={0}`, `{"A"}` | throws `IllegalArgumentException` | throws `IllegalArgumentException` | throws `IllegalArgumentException` |
| `quoted '{0}' plain {0}`, `{"A"}` | `quoted {0} plain A` | `quoted {0} plain A` | `quoted {0} plain A` |
| `miss={0} {1}`, `{"A"}` | `miss=A {1}` | `miss=A {1}` | `miss=A {1}` |
| `ten={10}`, `{"A"}` | `ten={10}` | `ten={10}` | `ten={10}` |

Single-quote suppression, the out-of-range index left verbatim, and the throw
on a non-numeric argument name are all behaviours that the two Rust
`MessageFormat.format` natives in this tree do **not** have — `p57_message_format`
in `native-builtins/src/phases_late/text_intl.rs` is a positional
`String::replace` loop and `p52_message_format_apply` in
`phases_early.rs` emits `{x}` literally instead of throwing. Neither could
produce this table. **The real `java.text.MessageFormat` bytecode is what runs,
in both modes, and it is HotSpot-exact on every case measured.**

**Verdict: cause A.** The dependency was healthy and the shadow simply never
called it. The body was:

```rust
match ctx.invoke_virtual(*rec, "getMessage", "()Ljava/lang/String;", &[])? {
    Some(Value::Object(Some(message))) => Ok(Some(Value::Object(Some(message)))),
    _ => Ok(Some(Value::Object(Some(ctx.create_string(""))))),
}
```

There is no guard to be wrong. There is no formatting at all. The trap in the
brief — "the guard is the most likely place for the bug" — is a fair prior and
it is not what happened here; the method had never had a step 4 to get wrong.

## 3. How many registrars

**One.** `native-builtins/src/phases_early.rs`, inside
`register_phase54_logging_extras`. A tree-wide search for the method name
returns that one `r.register` call, one comment above it, one comment in the
`SimpleFormatter` removal note below it, the fixture, the frozen kind-map
baseline row, and prose in five records — no second registration in any crate.
So this is not the last-write-wins shape where patching one of two registrars
is indistinguishable from patching none.

The row's category is worth stating precisely, because the record that
predicted this defect (`W7-35-jul-supplier-and-payload-residuals.md` §5)
prescribed a fix that does **not** by itself remove the shadow, and that
prescription has since landed:

* The row used to inherit `register_phase54_logging_extras`' ambient
  `Intrinsic`. `Intrinsic` is exempt from the `java/util/logging/` shadow
  retirement, so it survived when the retirement refused 84 sibling rows.
* W7-35 §5's patch moved it into a `with_category(Bridge)` block "so the
  retirement can see it". **That landed, and the row still dispatched.** The
  retirement is not kind-driven in the direction that patch assumed: it is the
  explicit `RETIRED_SHADOW_TRIPLES` table in `native-api/src/retired_shadow.rs`
  that re-tags a triple to `SyntheticStub`, and this triple was never added to
  it. Re-tagging to `Bridge` makes a row *eligible* to be listed; it does not
  list it.
* Even had it been listed, that closes `--jdk-only` only. `SyntheticStub`
  "registers and dispatches normally in `Compatible` mode" — the retirement
  file says so in its own header — and `formatMessage` is wrong in
  `Compatible` too.

This is the third instance in this codebase of *a page's prescribed fix being
wrong while its diagnosis was right*. W7-35's diagnosis was exactly right, down
to naming `MessageFormat` and the `{n}` scan.

## 4. The fix

`jul_formatter_format_message` in `native-builtins/src/phases_early.rs`,
reproducing JDK 25 `java.logging/java/util/logging/Formatter.java` read out of
the image's `src.zip` — not from memory — step for step:

1. `String format = record.getMessage();`
2. localize through `record.getResourceBundle()` when there is one, keeping the
   original when the lookup misses (`MissingResourceException` is the
   documented drop-through).
3. `record.getParameters()`; **null or zero-length returns the message with no
   formatting at all**, which is why a lone `{0}` in an unparameterized message
   must survive to the output.
4. the cheap `indexOf('{')` + next-char-is-a-digit scan, including the
   `index >= fence` break that makes a `{` in the last position not a pattern.
   The JDK's own comment explains why it is not `Pattern.compile("\\{\\d")`:
   the regex costs 14% more.
5. `java.text.MessageFormat.format(format, parameters)`, **and if it throws,
   return the original message** — the swallow of §2, reproduced, because it is
   load-bearing: `set={x} v={0}` reaches step 5, `MessageFormat` throws
   `IllegalArgumentException`, and HotSpot's answer is the raw string.

Step 4 is byte-wise rather than `char`-wise, and that is equivalence rather
than approximation: `{` and `0`-`9` are ASCII, and UTF-8 never encodes an ASCII
byte inside a multi-byte sequence.

Which of the JDK's calls are inside its `try` is reproduced too, not smoothed
over: `getMessage` and `getResourceBundle` are outside it and their exceptions
propagate; `getParameters` is inside it and its exceptions return the message.

### Both modes, deliberately

`Compatible` (`--real-jdk`) is contractually frozen except for genuine bug
fixes. **This is applied to both modes and that is intentional**: the row was
answering wrong in `Compatible` as well (measured, §5), the correct answer is
HotSpot's, and W7-35 §0 already scored `#54` as a `--real-jdk` failure. A
change that fixed only `--jdk-only` would leave the frozen mode frozen around a
defect.

### One deliberate deviation

`formatMessage(null)` is an NPE on HotSpot — `record.getMessage()` is the
method's first act. This row has answered with an empty string since it was
written, nothing measured exercises it, and introducing a throw that no test
can adjudicate is not a bug fix. The empty string is kept, and it is kept
**explicitly**, with the HotSpot answer named at the site.

## 5. The rest of the Formatter surface, swept

`SimpleFormatter` and `Formatter`, same binary, same three arms. Records built
by hand so the record side is not a variable.

| probe | HotSpot | CratonVM both modes | verdict |
|---|---|---|---|
| `formatMessage`, params + `{n}` | `one=A two=B` | `one={0} two={1}` | **the defect, fixed** |
| `formatMessage`, params but no `{n}` | `no placeholder here` | `no placeholder here` | correct — accidentally, it returned the message either way |
| `formatMessage`, `{0}` but NO params | `raw={0}` | `raw={0}` | correct, same accident |
| `formatMessage`, `set={x} v={0}` | `set={x} v={0}` | `set={x} v={0}` | correct via the swallow after the fix; was correct by accident before it |
| `formatMessage`, trailing lone `{` | `trail{` | `trail{` | correct — the `fence` case |
| `formatMessage`, **null message** | `null` | `""` | **second defect, same row, fixed** — `""` is not `null` and prints differently through `SimpleFormatter.format` |
| `formatMessage`, **ResourceBundle hit** | `localized Q` | `key.a` | **third defect, same row, fixed** — the row never resolved the bundle |
| `formatMessage`, ResourceBundle miss | `missing.key` | `missing.key` | correct — the drop-through, by the same accident |
| `Formatter.getHead(null)` | `""` | `""` | correct (real bytecode; no native) |
| `Formatter.getTail(null)` | `""` | `""` | correct (real bytecode; no native) |
| `SimpleFormatter.format(record)` | `<date> C m\nINFO: hello\n` | same shape | correct (real bytecode; the stale native was removed long ago) |
| `java.util.logging.SimpleFormatter.format` system property, unset | default template | default template | correct |
| `java.util.logging.SimpleFormatter.format=LVL-%4$s-MSG-%5$s%n` | `LVL-INFO-MSG-hello` | `LVL-INFO-MSG-hello` | **correct** — the property is read, from the system property, and drives the template |

Three defects, all three in the one row, all three fixed by §4. The rest of the
surface is real bytecode and is right.

One non-defect worth recording so the next reader does not chase it: the date
field of `SimpleFormatter.format` renders `Aug 12, 2026` on CratonVM and a
localized form on the HotSpot arm. That is the two runtimes' **default locale**
differing on this host, not a formatter divergence — the template, the field
order and the `%n` are identical, and forcing the template with the system
property makes the two arms byte-identical.

`ResourceBundle.getString` was checked as a dependency of step 2 rather than
assumed: it returns the localized value and raises
`java.util.MissingResourceException` on a miss, identically on HotSpot and on
both CratonVM modes.

## 6. What is proven and what is not

**Proven by running the pre-built binary**, HotSpot as the oracle on every arm,
every measurement taken **before** this lane's source change:

* the failing check and its exact text;
* that the record side is correct (raw pattern, both parameters);
* that `java.text.MessageFormat.format` is the real implementation and is
  HotSpot-exact on five patterns including the throw — **this is what makes the
  cause-A verdict a measurement rather than a reading**;
* every row of §5's table, on three arms;
* that `ResourceBundle.getString` and its `MissingResourceException` work.

**Proven by reading**, not by running:

* that there is exactly one registrar (a tree-wide search for the method name);
* that `RETIRED_SHADOW_TRIPLES` does not contain this triple, and that a
  `SyntheticStub` re-tag would not have reached `Compatible` anyway.

**Not proven here, and it is this lane's only open risk:** the fixed binary was
not built or run — this lane writes code and docs, the orchestrator builds. The
acceptance criterion is `RJdkLogging` reaching `PASS` under `--jdk-only` and
`--real-jdk` gaining no new failure. The guard of step 4 is covered by a unit
test (`jul_format_guard_matches_the_jdk_indexof_scan`) whose every row is a
HotSpot-measured answer and which can fail; the steps around it are covered
only by the fixture.

## 7. What this does NOT change, and why that is deliberate

* **No new `CRATONVM_*` flag.** Nothing here is a dial.
* **No census re-freeze.** The row count, the receiver and the kind are all
  unchanged — this replaces a body. `scripts/baselines/jdk-only-kind-map-25-linux.tsv`
  still carries the row as `intrinsic` and this change does not move it; that
  drift is the earlier `Intrinsic`→`Bridge` re-tag's to re-freeze, on `25/linux`,
  and it is not this lane's platform.
* **The row is still a shadow.** The strictly better end state is the one §1.4
  prescribes and W7-35 §5 aimed at: retire the registration entirely and let
  the real `Formatter.formatMessage` bytecode run, which would deliver every
  step above for free. Every dependency it needs was measured healthy here in
  both modes — `getMessage`, `getParameters`, `getResourceBundle`,
  `ResourceBundle.getString`, and a HotSpot-exact `MessageFormat`. What is
  missing is not evidence for the yield, it is a measured blast radius for
  removing a row that four suites' JULI paths call, which is a census-and-suite
  wave and not a rider on the last red. Recorded here as the successor item
  with its evidence already gathered.
