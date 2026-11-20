# CratonVM — Installation Guide

## Pre-built Binaries

Download the latest release from [GitHub Releases](https://github.com/craton-co/CVM/releases).

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

Requires **Rust 1.95+** and optionally **JDK 17+** (for compiling test Java classes).

```bash
git clone https://github.com/craton-co/CVM.git
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
| **Real JDK** (default) | Loads real JDK classes from `java.base.jmod` (or `lib/modules` on a jlink image) discovered via `--java-home`, `JAVA_HOME`, `CRATONVM_JAVA_HOME`, or `java` on `PATH`. |
| **Synthetic** (`--synthetic-jdk`, build-time opt-in) | Optional standalone Rust library (`--features synthetic-jdk`, not in default builds). |

At startup the launcher locates a real JDK runtime image (`jmods/java.base.jmod` or `lib/modules`). Default builds require an OpenJDK image; passing `--synthetic-jdk` requires a binary compiled with `--features synthetic-jdk`.

### Optional synthetic build mode

When compiled with `--features synthetic-jdk`, you can run with the synthetic library for hermetic tests or side-by-side comparison:

```bash
cratonvm --synthetic-jdk --classpath . HelloWorld
```

### Pointing at a specific JDK

```bash
cratonvm --java-home /path/to/jdk-25 --classpath . HelloWorld
```

## Troubleshooting

See [TROUBLESHOOTING.md](TROUBLESHOOTING.md) for common issues and solutions.
