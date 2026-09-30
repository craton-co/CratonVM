# cratonvm-reader

Java `.class` file parser for the CratonVM project.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Decodes Java class files according to the JVM specification, covering class
file versions from Java 1.1 (major 45) through Java 25 (major 69). Handles
the full constant pool (all 20 tag types), 200+ bytecode opcodes, 30+
attribute kinds, `StackMapTable` verification frames, generic signatures,
and JIMAGE (`modules`) archive layout used by modern JDK distributions.

## Non-goals

- No class loading, linking, or initialization (see `cratonvm-classloading`).
- No verification beyond structural decoding (see `cratonvm-classloading`'s
  verifier passes).
- No execution: this crate produces inert in-memory `ClassFile` data only.

## Usage

```rust
use cratonvm_reader::read_class;

let bytes = std::fs::read("Hello.class")?;
let class_file = read_class(&bytes)?;
println!("{} (major {})", class_file.this_class, class_file.major_version);
# Ok::<_, Box<dyn std::error::Error>>(())
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
