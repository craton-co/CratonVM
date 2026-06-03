# BreakIterator "Cannot load from null array" in real-JDK mode

## Symptom

JUnit Platform console `--help` aborts with:

```
java.lang.NullPointerException: Cannot load from null array
  at sun.util.locale.provider.BreakIteratorProviderImpl.getBreakInstance(BreakIteratorProviderImpl.java:170)
  at java.text.BreakIterator.createBreakInstance(BreakIterator.java:575)
  at java.text.BreakIterator.createBreakInstance(BreakIterator.java:563)
  at java.text.BreakIterator.getBreakInstance(BreakIterator.java:554)
  ...picocli CommandLine$Help$TextTable.copy / putValue / addRowValues...
```

Repro:

```
./target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 \
  -jar .bench-cache/junit-platform-console-standalone-1.10.2.jar --help
```

(Reproduces with JIT on *and* with `CRATONVM_DISABLE_JIT=1`, so it is not a JIT
codegen bug.)

## Root cause

picocli's help-text wrapper (`CommandLine$Help$TextTable.copy`) calls the
no-arg static factory `java.text.BreakIterator.getLineInstance()` to find
line-break boundaries. The real JDK 25 bytecode for that factory routes through:

```
BreakIterator.getLineInstance()
  -> BreakIterator.getBreakInstance(locale, LINE_INDEX)        // private, BreakIterator.java:544
    -> BreakIterator.createBreakInstance(locale, type)         // BreakIterator.java:559
      -> BreakIteratorProviderImpl.getBreakInstance(...)       // BreakIteratorProviderImpl.java:157
```

Inside `BreakIteratorProviderImpl.getBreakInstance` (JDK 25 line ~163-170):

```java
LocaleResources lr = LocaleProviderAdapter.forJRE().getLocaleResources(locale);
String[] classNames = (String[]) lr.getBreakIteratorInfo("BreakIteratorClasses");
...
switch (classNames[type]) {   // <-- line 170: aaload on classNames
```

On CratonVM, `lr.getBreakIteratorInfo("BreakIteratorClasses")` returns **null**
because `jdk.localedata`'s class-based locale resource bundles
(`sun.text.resources.**.BreakIteratorInfo_*`) are not surfaced through
CratonVM's jimage/resource path. The `switch (classNames[type])` then runs an
`aaload` on the null `classNames` array, producing
`NullPointerException: Cannot load from null array`. **The null array is
`classNames` at `BreakIteratorProviderImpl.java:170`** — not a BreakIterator
rule-data byte[]; the provider never even gets as far as loading rule data.

### Why CratonVM's BreakIterator natives did not fire

CratonVM already has a complete, working Rust BreakIterator implementation in
`native-builtins/src/phases_late.rs` (`register_p66_break_iterator`): real
boundary analysis for word/line/sentence/character plus the instance methods
`setText` / `first` / `next` / `previous` / `following` / `preceding` / `last`.
There is also an allow-list (`BREAKITER`) entry in
`vm/src/vm/vm_exec.rs` that promotes those natives over JDK bytecode.

The problem: `register_p66_break_iterator` is only reached via
`register_synthetic_overrides`, which is gated behind
`#[cfg(feature = "synthetic-jdk")]`. **Real-JDK CLI runs (`--java-home ...`) do
NOT compile that feature**, so the natives were never registered, the
allow-list's `native_methods.find(...)` returned `None`, `check_override` did
nothing, and the real (broken, null-resource) JDK bytecode ran.

## Fix

Register the existing BreakIterator factory + instance natives from the
real-JDK path as well, by calling `register_p66_break_iterator(registry)` at the
end of `register_essential_natives` (`native-builtins/src/lib.rs`).

This is **not** a synthetic fake-main shim: the iterator does genuine
Unicode-aware boundary analysis and is the same code already used (and proven)
in synthetic-jdk mode. It simply needs to be available in real-JDK mode too,
where the JDK's own provider path is non-functional because the locale rule
resources are missing.

`alloc_concurrent_synthetic` allocates the *real* abstract
`java.text.BreakIterator` class sized for `max(3, real_field_count)` slots
(real BreakIterator declares 0 instance fields, so 3 slots), and the
instance-method natives route via the `method.is_abstract()` branch of the
`check_override` logic in `vm_exec.rs` because `setText`/`first`/`next` are
abstract on the real class.

picocli's exact usage — `getLineInstance()`, `setText(String)`, `first()`,
`next()` — is all covered.

## Files changed

- `native-builtins/src/lib.rs` — call `register_p66_break_iterator` from
  `register_essential_natives` (real-JDK path).

No other files were modified. The fix stays within the LOCALE agent's allowed
surface (`vm/src/vm/vm_exec.rs` + `native-builtins/src/**`); the `vm_exec.rs`
allow-list entry already existed and needed no change.
