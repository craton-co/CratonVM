# Mapped JAR and shared class-byte storage

Classpath JARs, WARs, and JMODs are now opened as read-only memory mappings.
The `ZipArchive`, nested archive cursors, stored ZIP entries, class reader, lazy
attributes, and bounded `ClassManager` byte cache retain reference-counted
views of the same immutable backing.

Stored entries therefore require no archive-sized read and no class-sized
copy. Deflated entries are inflated into `SharedBytes`; parsing and the class
byte cache share that allocation instead of copying it again. Resource APIs
whose public contract returns `Vec<u8>` still materialize an owned result at
the API boundary.

The mapping is retained by every derived view, and archive metadata is checked
before and after mapping. JVM classpath archives must be immutable after loader
creation. Deployments that replace or truncate live classpath files must set
`CRATONVM_DISABLE_JAR_MMAP=1`, which selects the stable copied-file fallback.
Mapping failures also fall back automatically.

Manifest `Class-Path` cycles are deduplicated by archive origin before another
mapping or central-directory index is created.

Focused validation:

- reader unit suite, including shared lazy attribute views;
- classloading unit suite;
- stored-entry test proving external mapped storage;
- deflated-entry test proving reference-counted owned storage;
- manifest cycle test proving one mapping/index per archive.
