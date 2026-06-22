# Modules (JPMS)

CratonVM supports the Java Platform Module System (JPMS, JEP 261). You can run
modular applications from a module path and adjust the module graph with the
standard `--add-*` flags.

> Full JEP 261 resolution semantics are still maturing — see the
> [Roadmap](../contributing/roadmap.md). For most classpath-based applications
> you do not need any of these flags.

## The module path

Use `--module-path` (or `-p`) to point at directories and modular JARs:

```bash
cratonvm --module-path mods --add-modules com.example.app \
         --classpath . com.example.app.Main
```

The path separator is `;` on Windows and `:` on Unix, the same as the
classpath.

## Adjusting the module graph

| Flag | Form | Purpose |
|------|------|---------|
| `--add-modules` | `<module>` or `ALL-MODULE-PATH` | Add root modules to resolve. `ALL-MODULE-PATH` resolves everything on the module path. |
| `--add-reads` | `<module>=<target>[,<target>...]` | Add a read edge so one module can read another. |
| `--add-exports` | `<module>/<package>=<target>` | Export a package to another module (or `ALL-UNNAMED` for classpath code). |
| `--add-opens` | `<module>/<package>=<target>` | Open a package for deep reflection at runtime. |

All four can be repeated; entries from every occurrence accumulate.

### Examples

```bash
# Open an internal package to reflection-heavy frameworks on the classpath
cratonvm --add-opens java.base/java.lang=ALL-UNNAMED \
         --classpath app.jar com.example.Main

# Export an internal API to a specific module
cratonvm --module-path mods \
         --add-exports com.example.core/com.example.core.internal=com.example.plugin \
         --add-modules com.example.app \
         com.example.app.Main
```

## Native access for modules

The `--enable-native-access` flag grants modules permission to call restricted
`java.lang.foreign` (Panama) methods. See the
[Command-Line Reference](cli-reference.md#native-access-panama--javalangforeign).
