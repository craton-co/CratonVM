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

Requires **Rust 1.75+** and optionally **JDK 17+** (for compiling test Java classes).

```bash
git clone https://github.com/craton-co/cratonvm.git
cd cratonvm
cargo build --release -p cratonvm-cli
```

The package is `cratonvm-cli` but the binary it produces is `cratonvm`, so the
executable lands at `target/release/cratonvm` (or `cratonvm.exe` on Windows).

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

| Mode | Flag | Description |
|------|------|-------------|
| **Synthetic** (default) | `--synthetic-jdk=true` | Uses built-in Rust implementations of Java stdlib. No JDK needed. |
| **Real JDK** | `--synthetic-jdk=false` | Loads real JDK classes from JMOD files. Requires `--java-home` or `JAVA_HOME`. |

### Real JDK mode

```bash
cratonvm --synthetic-jdk=false --java-home /path/to/jdk-25 --classpath . HelloWorld
```

## Troubleshooting

See [TROUBLESHOOTING.md](TROUBLESHOOTING.md) for common issues and solutions.
