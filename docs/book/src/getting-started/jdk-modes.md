# JDK Modes: Real vs. Synthetic

One of CratonVM's defining traits is that it can run **with or without a real
JDK installed**. Understanding which mode you are in explains a lot about
behavior and coverage.

## The two modes

| Mode | When it's used | What provides the standard library |
|------|----------------|------------------------------------|
| **Real JDK** | The `cratonvm` launcher default, or `--real-jdk` | Real `java.base` bytecode loaded from the JDK's `java.base.jmod` (or `lib/modules` on a jlink image), backed by CratonVM's native methods |
| **Synthetic** | `--synthetic-jdk`, and the in-process embedding default | CratonVM's own Rust implementations of standard-library classes — no JDK required |

In **real-JDK mode**, CratonVM loads the actual JDK class files and runs their
bytecode, supplying the underlying `native` methods (the ones HotSpot would
implement in C) from its Rust native registry. This is the standard,
highest-fidelity path.

In **synthetic mode**, the standard-library classes are themselves Rust
implementations. This is what lets CratonVM run with **no JDK, no `JAVA_HOME`,
and no `rt.jar`** at all — useful for hermetic, self-contained tooling and for
comparing the two backends side by side.

## How the mode is chosen

The mode comes from an explicit flag (`--real-jdk` / `--synthetic-jdk`) or from
a **fixed default** — never from what the host happens to have installed. The
`cratonvm` launcher defaults to real-JDK; the in-process library default
(`VmConfig::default()`) stays synthetic so hermetic tests and embedders that
ship no JDK do not start resolving JMODs from the build machine's JDK.

Once real-JDK mode is selected, the launcher searches for the installation to
boot from, in order:

1. The `--java-home` flag, if given.
2. The `CRATONVM_JAVA_HOME` environment variable.
3. The `JAVA_HOME` environment variable.
4. A `java` executable on your `PATH`.

It needs a JDK containing `jmods/java.base.jmod` (or `lib/modules`). If it finds
none, the launch **fails** with a message naming everything it searched — it
does not silently substitute the synthetic library, because a run whose standard
library was chosen by the host is neither reproducible nor reportable. The same
applies in reverse: `--synthetic-jdk` on a binary built without the
`synthetic-jdk` Cargo feature is an error, not a downgrade.

## Forcing a mode

**Force synthetic mode** even when a JDK is present (hermetic runs, backend
comparison):

```bash
cratonvm --synthetic-jdk --classpath . HelloWorld
```

`--synthetic-jdk` wins over `--java-home`, and conflicts with both `--real-jdk`
and `--jdk-only`.

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

- **For maximum compatibility**, run in real-JDK mode (the launcher default).
  You get the real standard-library bytecode.
- **For a self-contained binary** with no JDK dependency, force synthetic mode
  with `--synthetic-jdk`, on a build compiled with the `synthetic-jdk` feature.

Real-JDK mode is the project's primary direction; synthetic implementations are
the standalone fallback and a tool for differential testing. Coverage and a few
behavior switches differ between the two — see [Standard Library
Coverage](../java-support/standard-library.md) and
[Configuration](../user-guide/configuration.md).

There is a third choice on top of the library selection: whether the VM may
substitute for what the library does not provide. `--jdk-only` says it may not.
See [JDK-Only Mode](../user-guide/jdk-only-mode.md).
