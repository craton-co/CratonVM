# Running Programs

This chapter covers the everyday mechanics of launching Java code: classpaths,
JARs, program arguments, system properties, and argument files. For the
exhaustive flag list see the [Command-Line Reference](cli-reference.md).

## Invocation forms

```text
cratonvm [OPTIONS] <CLASS_NAME> [ARGS...]
cratonvm [OPTIONS] --jar <FILE.jar> [ARGS...]
```

- `<CLASS_NAME>` is a fully-qualified class name with **dots**
  (`com.example.Main`). CratonVM does not auto-detect a main class from a bare
  classpath — name the class explicitly, or use `--jar`.
- Tokens after the class name (or after `--jar <FILE>`) are passed to your
  program's `main(String[])`.

## The classpath

Use `--classpath` (aliases: `-cp`, `-c`, `--cp`) to list directories and JAR
files to search for `.class` files. The entry separator is platform-specific:

- **Windows:** `;`
- **Unix/macOS:** `:`

```bash
# A single directory (the default is the current directory)
cratonvm --classpath . MyApp

# A directory plus a JAR, on Unix
cratonvm --classpath "build/classes:lib/utils.jar" com.example.Main

# The same on Windows
cratonvm --classpath "build\classes;lib\utils.jar" com.example.Main
```

If you pass `--classpath` more than once, the last one wins (it does not
accumulate). Combine entries into a single value instead.

## Running a JAR

```bash
cratonvm --jar app.jar arg1 arg2
```

The main class is read from the JAR's `../../../../apps/META-INF/MANIFEST.MF` (`Main-Class`
header). When `--jar` is used, `--classpath` is ignored.

## Program arguments

Everything after the class name (or after the `--jar` file) goes to your
program:

```bash
cratonvm --classpath . MyApp --name=Ada --count 3
```

Inside `MyApp.main(String[] args)`, `args` is `["--name=Ada", "--count", "3"]`.

If you ever need to make the boundary explicit — for example, to stop CratonVM
from interpreting a leading-dash program argument as one of its own options —
use `--` to terminate launcher options:

```bash
cratonvm --classpath . MyApp -- --this-goes-to-my-app
```

## System properties

Set Java system properties (readable via `System.getProperty`) with `-D`:

```bash
cratonvm -Dapp.env=prod -Duser.region=eu --classpath . MyApp
```

`-Dkey=value` sets the property to `value`; a bare `-Dkey` sets it to the empty
string. These are extracted before the rest of the command line is parsed.

## Argument files (`@file`)

For long command lines, CratonVM supports Java-style argument files (JEP 293):
pass `@path` and the file's contents are expanded as additional arguments.

```bash
cratonvm @run-args.txt MyApp
```

```text
# run-args.txt
--classpath build/classes:lib/dep.jar
--Xmx 1g
-Dapp.env=prod
```

Rules: comments start with `#`, quoted strings preserve whitespace, `@@path` is
a literal `@path`, and expansion is non-recursive (an argument file cannot
include another).

## Modules

CratonVM supports the Java Platform Module System. Use `--module-path` /`-p`,
`--add-modules`, `--add-reads`, `--add-exports`, and `--add-opens`. See
[Modules (JPMS)](modules.md).

## Picking the standard-library backend

By default CratonVM uses a real JDK if it finds one, and its synthetic
implementations otherwise. Force the choice with `--synthetic-jdk` or
`--java-home`. See [JDK Modes](../getting-started/jdk-modes.md).

## A note on heap size

When you do not pass `-Xmx`, the launcher picks an *ergonomic* default heap
(roughly a quarter of physical RAM, capped). Real-world frameworks need this —
a fixed small heap makes allocation-heavy apps thrash the collector and look
like a hang. See [Memory & Garbage Collection](memory-and-gc.md) for the exact
rules and how to override them.
