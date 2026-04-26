# RustJVM — Installation Guide

## Pre-built Binaries

Download the latest release from [GitHub Releases](https://github.com/craton-co/rust-jvm/releases).

| Platform | File | Notes |
|----------|------|-------|
| Linux x86-64 | `rustjvm-x86_64-linux-gnu.tar.gz` | Requires glibc 2.17+ |
| Windows x86-64 | `rustjvm-x86_64-windows-msvc.zip` | Requires MSVC runtime |
| macOS ARM64 | `rustjvm-aarch64-macos.tar.gz` | Apple Silicon (M1+) |

### Linux / macOS

```bash
# Download and extract
tar xzf rustjvm-*.tar.gz

# Move to a directory in your PATH
sudo mv rustjvm-cli /usr/local/bin/rustjvm

# Verify installation
rustjvm --help
```

### Windows

1. Download and extract the `.zip` file
2. Add the extracted directory to your `PATH` environment variable
3. Open a new terminal and verify:
   ```
   rustjvm --help
   ```

## Building from Source

Requires **Rust 1.75+** and optionally **JDK 17+** (for compiling test Java classes).

```bash
git clone https://github.com/craton-co/rust-jvm.git
cd rust-jvm
cargo build --release -p rustjvm-cli
```

The binary is at `target/release/rustjvm-cli` (or `rustjvm-cli.exe` on Windows).

See [BUILD_GUIDE.md](../BUILD_GUIDE.md) for detailed build instructions, testing, and benchmarking.

## Running Your First Program

### 1. Write a Java program

```java
// HelloWorld.java
public class HelloWorld {
    public static void main(String[] args) {
        System.out.println("Hello from RustJVM!");
    }
}
```

### 2. Compile with `javac`

```bash
javac HelloWorld.java
```

### 3. Run with RustJVM

```bash
rustjvm --classpath . HelloWorld
```

## JDK Mode

RustJVM can run in two modes:

| Mode | Flag | Description |
|------|------|-------------|
| **Synthetic** (default) | `--synthetic-jdk=true` | Uses built-in Rust implementations of Java stdlib. No JDK needed. |
| **Real JDK** | `--synthetic-jdk=false` | Loads real JDK classes from JMOD files. Requires `--java-home` or `JAVA_HOME`. |

### Real JDK mode

```bash
rustjvm --synthetic-jdk=false --java-home /path/to/jdk-25 --classpath . HelloWorld
```

## Troubleshooting

See [TROUBLESHOOTING.md](TROUBLESHOOTING.md) for common issues and solutions.
