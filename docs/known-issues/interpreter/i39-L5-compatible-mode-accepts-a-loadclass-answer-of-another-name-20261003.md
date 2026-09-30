# `--compatible` accepts a `loadClass` answer of another name, or `null` from `Class.forName` (JVMS §5.3.2)

**Status: open — filed 2026-10-03 by interpreter round i1 wave 39, lane L5
(review of class loading). `--jdk-only` is fixed (below); `--compatible` is
the owner's call (AGENTS.md: that mode stays byte-for-byte unless a probe
shows a genuine bug, and this is one, but every user-loader miss in that mode
still falls back to the global store by design, so the fix belongs with that
decision).**

## What happens

JVMS §5.3.2: when the VM asks a user-defined loader `L` for `N`, the answer
must be a class named `N`; HotSpot's `SystemDictionary` returns nothing
otherwise (`NoClassDefFoundError` at a resolution, `ClassNotFoundException:
<internal name>` from `Class.forName(N, init, L)`), and a `null` answer is the
same miss.

CratonVM `--compatible`, from the code (not run):

* **The resolution door** (`vm/src/runtime/interpreter/constants.rs`
  `drive_defining_loader_load_named`): any mirror `loadClass` returns is
  taken, so `new p.Target` whose loader returns `Object.class` binds
  `java.lang.Object` (probe `L5W39LoaderReturnsNull`, `wrong` rows: `ok`); a
  `null` answer falls back to the global store (`null` rows: `ok`).
* **`Class.forName(N, init, L)`** (`native-builtins/src/lang_class.rs`
  `native_class_for_name`): returns what `loadClass` returned — `null`
  (a later `NullPointerException` in the caller) or the other class (probe
  `L5W39ForNameWrongAnswer`).

## Fixed under `--jdk-only` (wave 39, lane L5)

* The resolution door compares the answer's name with the request
  (`wrong_name`); at the checked door, for a loader whose `loadClass` is its
  own bytecode, a `null` or wrong-named answer is
  `NoClassDefFoundError: <internal name>` (`loader_throw::
  loader_null_as_resolution_error`), recorded against the entry; the other
  drives treat it as no answer.
* `Class.forName` with a loader: `ClassNotFoundException: <internal name>`.

## What would fix it

`--compatible`: the same two checks, gated on the mode decision that retires
that mode's global fallback for user-loader misses
(`interpreter-L5-a-vm-initiated-loadclass-exception-is-swallowed-into-a-global-fallback-FIXED-20261005.md`).
The `Class.forName` half is independent of the fallback and could go first
with a `--compatible` census (`CRATONVM_DBG=access`-style count of
`forName` answers whose name differs, over the `--compatible` suites).
