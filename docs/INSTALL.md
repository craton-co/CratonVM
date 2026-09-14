# CratonVM — Installation Guide

## Pre-built Binaries

Download the latest release from [GitHub Releases](https://github.com/craton-co/cratonvm/releases).

| Platform | File | Notes |
|----------|------|-------|
| Linux x86-64 | `cratonvm-x86_64-linux-gnu.tar.gz` | Requires glibc 2.17+ |
| Windows x86-64 | `cratonvm-x86_64-windows-msvc.zip` | Requires MSVC runtime |
| macOS ARM64 | `cratonvm-aarch64-macos.tar.gz` | Apple Silicon (M1+) |

### Linux / macOS

```bash
# Download and extract
tar xzf cratonvm-*.tar.gz

# Move to a directory in your PATH
sudo mv cratonvm /usr/local/bin/cratonvm

# Verify installation
cratonvm --help
```

### Windows

1. Download and extract the `.zip` file
2. Add the extracted directory to your `PATH` environment variable
3. Open a new terminal and verify:
   ```
   cratonvm --help
   ```

## Building from Source

Requires **Rust 1.88+** and optionally **JDK 17+** (for compiling test Java classes).

```bash
git clone https://github.com/craton-co/cratonvm.git
cd cratonvm
cargo build --release -p cratonvm-cli
```

The package is `cratonvm-cli` but the binary it produces is `cratonvm`, so the
executable lands at `target/release/cratonvm` (or `cratonvm.exe` on Windows).

### Optional `java` binary alias

CratonVM intentionally does **not** install a `java` binary by default —
that would shadow the system JDK launcher (`~/.cargo/bin/java` on the
PATH is a footgun, and `cargo install cratonvm-cli` would silently
replace your real `java`). If you need a `java[.exe]` launcher (e.g.
for Maven Surefire's `-Djvm=...` validation which requires the binary
basename to be `java`), opt in with the `java-bin-alias` feature:

```bash
cargo build --release -p cratonvm-cli --features java-bin-alias
# now both target/release/cratonvm and target/release/java exist
```

See [BUILD_GUIDE.md](../BUILD_GUIDE.md) for detailed build instructions, testing, and benchmarking.

## Running Your First Program

### 1. Write a Java program

```java
// HelloWorld.java
public class HelloWorld {
    public static void main(String[] args) {
        System.out.println("Hello from CratonVM!");
    }
}
```

### 2. Compile with `javac`

```bash
javac HelloWorld.java
```

### 3. Run with CratonVM

```bash
cratonvm --classpath . HelloWorld
```

## JDK Mode

CratonVM can run in two modes:

| Mode | Description |
|------|-------------|
| **Real JDK** (default when a JDK is present) | Loads real JDK classes from `java.base.jmod` (or `lib/modules` on a jlink image) discovered via `JAVA_HOME`, `CRATONVM_JAVA_HOME`, or `java` on `PATH`. |
| **Synthetic** (default when no JDK is detected, or with `--synthetic-jdk`) | Uses built-in Rust implementations of the Java stdlib. No JDK needed. |

At startup the launcher probes the host for a real JDK (see
`detect_real_jdk` in `vm/src/config.rs`). When `jmods/java.base.jmod`
or `lib/modules` is found, the VM boots from that. When no JDK is
detected, it falls back to the synthetic stubs.

### Forcing synthetic mode

Even with a JDK on the host you can force the synthetic path — useful
for hermetic test runs or for comparing the two backends side-by-side:

```bash
cratonvm --synthetic-jdk --classpath . HelloWorld
```

### Pointing at a specific JDK

```bash
cratonvm --java-home /path/to/jdk-25 --classpath . HelloWorld
```

## Troubleshooting

See [TROUBLESHOOTING.md](TROUBLESHOOTING.md) for common issues and solutions.
