# `--jdk-only`: `LogRecord.inferCaller()` produces nothing, so `SimpleFormatter` renders the logger name

| | |
|---|---|
| **Status** | SUPERSEDED 2026-08-12 by W7-56-infercaller-strict.md, which measured it and fixed it. **Both candidate causes in "where to look first" below are REFUTED**: the setters stick, and our `StackWalker` presents `CallerFinder` exactly the frames HotSpot's walks. The cause was a third thing — the shadow `getSourceClassName` is the real getter with the `inferCaller()` call deleted, so the working mechanism was never CALLED. Read that record instead of the section below. |
| **Discovered** | 2026-08-12, closing the `RJdkLogging` `useParentHandlers` failure — this is the last of four defects that vector stacks up. |
| **Reproducer** | `CRATONVM_ARGS=--jdk-only SUITE=all bash regression-suite/run.sh` → `69 passed, 1 failed ( failed: RJdkLogging )`, on `formattedOutputIsRealBytes`. |
| **Oracle** | HotSpot renders `RJdkLogging formattedOutputIsRealBytes`. |

## The symptom

```
AssertionError: SimpleFormatter must render the inferred source class and method;
got [Aug 12, 2026 3:18:45 AM rjdklogging.stream
```

`rjdklogging.stream` is the LOGGER NAME. `SimpleFormatter`'s default pattern
renders the record's source class and method, and its documented fallback when
the record has neither is the logger name — so this is not a formatting bug, it
is a record that arrived with a null source pair and a formatter politely
papering over it.

Reduced (`SrcProbe2`: a `Handler` that reads the pair *during* `publish`, which
is when `SimpleFormatter` reads it):

| | during publish | formatted |
|---|---|---|
| HotSpot | `SrcProbe2 / main` | `… SrcProbe2 main\|WARNING: MARK2` |
| CratonVM `--real-jdk` | `SrcProbe2 / main` | `… SrcProbe2 main\|WARNING: MARK2` |
| **CratonVM `--jdk-only`** | **`null / null`** | `… srcprobe2.one\|WARNING: MARK2` |

## Why the Compatible fix does not reach it

In `Compatible` the whole `Logger.warning → publish` chain is native, so the
bridge stamps the pair itself at record construction — where the innermost Java
frame *is* the caller (`logmanager.rs`, `stamp_inferred_caller`).

Under `--jdk-only` every one of those natives is refused (47
`java/util/logging/Logger` triples are in `retired_shadow.rs`) and the REAL
bytecode runs: the real constructor sets `needToInferCaller = true`, the real
`getSourceClassName()` calls the real `inferCaller()`, and the real
`Logger.warning`/`log`/`doLog` frames are on the stack for it to walk. Every
precondition HotSpot needs is present, and the answer is still null. So the
defect is inside `inferCaller()`'s own mechanism on CratonVM, not in the
logging bridge.

## Where to look first

`LogRecord.inferCaller()` walks with `StackWalker`:

```java
private void inferCaller() {
    needToInferCaller = false;
    Optional<StackWalker.StackFrame> frame = new CallerFinder().get();
    frame.ifPresent(f -> { setSourceClassName(f.getClassName());
                           setSourceMethodName(f.getMethodName()); });
}
```

`CallerFinder` is a `Predicate<StackFrame>` fed to
`StackWalker.walk`/`StackWalker.getInstance(...).walk(...)`, and it rejects
frames until it has passed the logging classes. Two candidate causes, neither
yet checked:

1. **`StackWalker` returns a stack our `Logger` frames are missing from** — the
   allow-listed natives in `Compatible` are refused here, but the *interpreter*
   may still not present the real `Logger.log`/`doLog` frames the way the walk
   expects.
2. **`setSourceClassName`/`setSourceMethodName` do not stick** — they are
   registered natives (`phases_early.rs`) and write through
   `log_record_real_layout`; if that predicate answers `false` for a
   bytecode-constructed record the write lands in a raw slot the real getter
   never reads.

Check (2) first: it is one `CRATONVM_DBG` print away, and it would also explain
why `getSourceClassName()` returns null rather than throwing.

## What is already fixed and must not be re-opened

Three sibling defects in the same vector were closed on 2026-08-12 and are
green in BOTH modes: `Formatter.formatMessage` not substituting `{n}`, the
missing `Logger.global` registration, and — `Compatible`-only —
`useParentHandlers` being written to a side table nothing read.
