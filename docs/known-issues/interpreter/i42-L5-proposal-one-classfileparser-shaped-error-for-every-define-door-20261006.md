# Proposal: one `ClassFileParser`-shaped error for every define door

**Status: proposal — filed 2026-10-06 by interpreter round i1 wave 42, lane
L5. Not implemented.**

## The problem

A malformed class file reaches CratonVM through at least six doors, and each
spells its own `ClassFormatError`:

* `ClassLoader.defineClass1` / `defineClass2` (`native-builtins/src/lang_system.rs`):
  `validate_classfile_header` ("<name>: defineClass1: not a valid class file
  (bad magic)"), then the backend's text;
* `ClassLoader.defineClass0`, `Unsafe.defineClass`,
  `Unsafe.defineAnonymousClass`, `Lookup.defineClass` /
  `defineHiddenClass` (`classloader.rs`, `unsafe_natives.rs`,
  `lookup_define.rs`): each has its own "…: not a valid class file (bad
  magic)" copy;
* the class manager's parse (`classloading/src/class_manager.rs`,
  `define_class_shared_with_options`): `ClassReaderError`'s `Display` text
  ("unexpected end of data at position 10", "invalid constant pool tag: 19
  (at index 4)", …), prefixed with the class name by the throwable builder
  (`exceptions.rs`: `"{class_name}: {message}"`);
* class redefinition and the agent / JVMTI define paths, through the same
  class-manager parse.

HotSpot has one source, `ClassFileParser`, and its messages are stable text a
container or a test can match: "Truncated class file", "Incompatible magic
value N in class file X", "Illegal constant pool type N in class file X",
"Invalid constant pool index N in class file X", "Duplicate field name …",
and so on.

Wave 42 (lane L5) made `defineClass1` / `defineClass2` raise HotSpot's three
header messages and "Truncated class file" under `--jdk-only`
(`jdk_only_classfile_header_error`, `jdk_only_truncated_class_file`; probe
`tools/probes/interp/L5/L5W42DefineClassErrors.java`). That is a patch at
two doors that recognises the reader's error by its `Debug` text; the other
doors and the constant-pool / field / method errors still say CratonVM's
words.

## The direction

1. Give `ClassReaderError` a `hotspot_message(class_name: &str) -> String`
   (as `unsupported_class_version_message` already does for the version
   rejection), covering the reader's variants with `ClassFileParser`'s text,
   `<Unknown>` for a missing name. Measure each against JDK 25 with a probe
   that hand-builds the malformed file (one row per variant).
2. The class manager builds its `LinkageError::ClassFormatError` from that
   text, with a flag on the variant that tells the throwable builder not to
   prefix the class name (the message already names it, as
   `UnsupportedClassVersionError`'s arm does).
3. Every native door drops its own magic check and lets the parse report it
   (the check exists only so the doors fail before side effects; move it to
   one shared `precheck_class_bytes(name, bytes) -> Result<(), VmError>` that
   returns the same typed error), and the `Debug`-string recovery
   (`typed_define_class_error`) keeps working unchanged because the variant
   is still `ClassFormatError`.
4. `--compatible`: its wording is the owner's call (AGENTS.md); the switch
   can be the same `is_jdk_only()` test the wave-42 patch uses, removed once
   the owner accepts HotSpot's text in both modes.

## Cost and risk

Cold (a failing define). The risk is text only: a test that matches
CratonVM's current wording. `rg "not a valid class file"` over the
repository's tests before changing a door.
