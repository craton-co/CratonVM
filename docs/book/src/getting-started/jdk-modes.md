# JDK Modes: Real vs. Synthetic

One of CratonVM's defining traits is that it can run **with or without a real
JDK installed**. Understanding which mode you are in explains a lot about
behavior and coverage.

## The two modes

| Mode | When it's used | What provides the standard library |
|------|----------------|------------------------------------|
| **Real JDK** | Default whenever a JDK is detected on the host | Real `java.base` bytecode loaded from the JDK's `java.base.jmod` (or `lib/modules` on a jlink image), backed by CratonVM's native methods |
| **Synthetic** | Default when no JDK is detected, or forced with `--synthetic-jdk` | CratonVM's own Rust implementations of standard-library classes — no JDK required |

In **real-JDK mode**, CratonVM loads the actual JDK class files and runs their
bytecode, supplying the underlying `native` methods (the ones HotSpot would
implement in C) from its Rust native registry. This is the standard,
highest-fidelity path.

In **synthetic mode**, the standard-library classes are themselves Rust
implementations. This is what lets CratonVM run with **no JDK, no `JAVA_HOME`,
and no `rt.jar`** at all — useful for hermetic, self-contained tooling and for
comparing the two backends side by side.

## How the mode is chosen

At startup the launcher probes the host for a real JDK. It looks, in order, at:

1. The `--java-home` flag, if given.
2. The `CRATONVM_JAVA_HOME` environment variable.
3. The `JAVA_HOME` environment variable.
4. A `java` executable on your `PATH`.

If it finds a JDK containing `jmods/java.base.jmod` (or `lib/modules`), it boots
in **real-JDK mode** from that JDK. Otherwise it falls back to **synthetic
mode**.

> The library default (the in-process `VmConfig::default()`) keeps synthetic
> mode on for hermetic tests, but the `cratonvm` **launcher** flips to real-JDK
> mode automatically whenever it detects a JDK on the host.

## Forcing a mode

**Force synthetic mode** even when a JDK is present (hermetic runs, backend
comparison):

```bash
cratonvm --synthetic-jdk --classpath . HelloWorld
```

`--synthetic-jdk` wins over `--java-home`.

**Point at a specific JDK** for the boot/ext class path and JMOD loading:

```bash
cratonvm --java-home /path/to/jdk-25 --classpath . HelloWorld
```

**Use `CRATONVM_JAVA_HOME`** when a build tool (Maven, Gradle) has pointed
`JAVA_HOME` at a CratonVM shim tree, but the boot modules should still come from
a real JDK:

```bash
CRATONVM_JAVA_HOME=/path/to/jdk-25 cratonvm --classpath . HelloWorld
```

## Which mode should I use?

- **For maximum compatibility**, run in real-JDK mode (the default when a JDK is
  installed). You get the real standard-library bytecode.
- **For a self-contained binary** with no JDK dependency, rely on synthetic mode
  (it activates automatically when no JDK is found) or force it with
  `--synthetic-jdk`.

Real-JDK mode is the project's primary direction; synthetic implementations are
the standalone fallback and a tool for differential testing. Coverage and a few
behavior switches differ between the two — see [Standard Library
Coverage](../java-support/standard-library.md) and
[Configuration](../user-guide/configuration.md).
