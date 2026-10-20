# Installation

There are two ways to get CratonVM: download a pre-built binary, or build it
from source with the Rust toolchain.

## Pre-built binaries

Download the latest release from the project's
[GitHub Releases](https://github.com/craton-co/cratonvm/releases) page.

| Platform | Archive | Notes |
|----------|---------|-------|
| Linux x86-64 | `cratonvm-x86_64-linux-gnu.tar.gz` | Requires glibc 2.17+ |
| Windows x86-64 | `cratonvm-x86_64-windows-msvc.zip` | Requires the MSVC runtime |
| macOS ARM64 | `cratonvm-aarch64-macos.tar.gz` | Apple Silicon (M1 and later) |

### Linux / macOS

```bash
# Extract the archive
tar xzf cratonvm-*.tar.gz

# Move the binary somewhere on your PATH
sudo mv cratonvm /usr/local/bin/cratonvm

# Verify
cratonvm --help
```

### Windows

1. Extract the `.zip` archive.
2. Add the extracted directory to your `PATH` environment variable.
3. Open a new terminal and verify:

   ```text
   cratonvm --help
   ```

## Building from source

CratonVM builds with the standard Rust toolchain.

**Prerequisites**

- **Rust 1.88 or newer** — install via [rustup.rs](https://rustup.rs).
- **A JDK (17+) is optional** — needed only to compile Java test classes and to
  boot against a real `java.base`. CratonVM runs standalone without one.
- **Visual Studio Build Tools** (Windows only) — for the MSVC toolchain and
  linker.

```bash
git clone https://github.com/craton-co/cratonvm.git
cd cratonvm
cargo build --release -p cratonvm-cli
```

The package is `cratonvm-cli`, but the binary it produces is named `cratonvm`,
so the executable lands at `target/release/cratonvm` (or `cratonvm.exe` on
Windows).

For the full build, test, lint, and benchmark workflow, see
[Building from Source](../contributing/building.md).

### Optional `java` binary alias

CratonVM deliberately does **not** install a `java` binary by default — doing so
would shadow your system JDK launcher (and `cargo install` placing a `java` on
your `PATH` is a footgun). If you specifically need a `java[.exe]` launcher — for
example, for build tools that validate the launcher's basename — opt in with the
`java-bin-alias` Cargo feature:

```bash
cargo build --release -p cratonvm-cli --features java-bin-alias
# now both target/release/cratonvm and target/release/java exist
```

## Verifying the install

```bash
cratonvm --version
cratonvm --help
```

## Next steps

- [Run your first program](first-program.md).
- Understand [JDK modes](jdk-modes.md) — when CratonVM uses a real JDK vs. its
  built-in standard library.
- If something goes wrong, see [Troubleshooting](../user-guide/troubleshooting.md).
