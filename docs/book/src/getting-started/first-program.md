# Your First Program

This walkthrough takes you from a `.java` source file to running output on
CratonVM.

## 1. Write a Java program

```java
// HelloWorld.java
public class HelloWorld {
    public static void main(String[] args) {
        System.out.println("Hello from CratonVM!");
        for (String arg : args) {
            System.out.println("arg: " + arg);
        }
    }
}
```

## 2. Compile it

CratonVM runs *bytecode*, so you compile with a standard Java compiler
(`javac` from any JDK 8+):

```bash
javac HelloWorld.java
```

This produces `HelloWorld.class`.

## 3. Run it on CratonVM

```bash
cratonvm --classpath . HelloWorld
```

```text
Hello from CratonVM!
```

Pass program arguments after the class name:

```bash
cratonvm --classpath . HelloWorld one two three
```

```text
Hello from CratonVM!
arg: one
arg: two
arg: three
```

> **Note:** class names use dots, not slashes or file paths:
> `com.example.Main`, not `com/example/Main` or `com/example/Main.class`.

## Running from the source tree

If you built from source and have not installed the binary, you can run it
directly through Cargo:

```bash
cargo run --release -p cratonvm-cli -- --classpath . HelloWorld
```

Everything after `--` is passed to CratonVM verbatim. Always use `--release`
for anything performance-sensitive — debug builds are 10–50× slower.

## Running a JAR

If your program is packaged as a JAR with a `Main-Class` in its manifest, run it
with `--jar`. The classpath flag is ignored in this mode (the main class comes
from `../../../../apps/META-INF/MANIFEST.MF`):

```bash
cratonvm --jar app.jar arg1 arg2
```

## Common next options

| You want to… | Use |
|--------------|-----|
| Add libraries to the classpath | `--classpath "app:lib/dep.jar"` (`;` separator on Windows) |
| Give the program more heap | `--Xmx 2g` |
| Watch garbage collection | `--verbose:gc` |
| Watch class loading | `--verbose:class` |
| Disable the JIT (interpreter only) | `--nojit` |
| Set a system property | `-Dkey=value` |

See the [Command-Line Reference](../user-guide/cli-reference.md) for the full
list, and [Running Programs](../user-guide/running-programs.md) for classpaths,
JARs, modules, and argument files in depth.
