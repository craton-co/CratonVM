# Bug 06 — Discovery crash in `AnnotationUtils.findRepeatableAnnotations` (deep recursion)

**Severity:** High — `org.apache.kafka.clients.consumer.internals` fails discovery
and runs zero tests. `--nojit`. HotSpot runs the package clean.
(The same crash also appears for `clients` and `clients.consumer`, but those also
need a broker and HANG on HotSpot, so they are not counted as CVM-only there.)

**Symptom:**
```
org.junit.platform.commons.JUnitException: TestEngine 'junit-jupiter' failed to discover tests
  ... at org/junit/platform/commons/util/AnnotationUtils.findRepeatableAnnotations(AnnotationUtils.java:336)
      at org/junit/platform/commons/util/AnnotationUtils.findRepeatableAnnotations(AnnotationUtils.java:320)
      at org/junit/platform/commons/util/AnnotationUtils.findRepeatableAnnotations(AnnotationUtils.java:291)
      at JupiterTestDescriptor.getTags ... ClassTestDescriptor.<init>
```

`findRepeatableAnnotations` recurses (336 → 320 → 291) while resolving
`@Tag`/`@Tags` (repeatable) annotations on a test class. The recursion does not
terminate / overflows on CratonVM, where HotSpot resolves the same annotations
fine. Likely a CratonVM annotation-reflection defect: a meta-annotation cycle that
HotSpot breaks (via a visited-set or correct `@Repeatable` container resolution)
but CratonVM does not — possibly returning a self-referential annotation or a
wrong `annotationType()` so the visited-set never matches.

Related prior work: `reference_sb09_annotation_invoke_handler`,
`reference_constructor_annotation_reflection` — annotation proxy / reflection area.

## Next steps
- Minimal repro: a test class annotated with `@Tag` (and/or a custom
  `@Repeatable` annotation) → drive `AnnotationUtils.findRepeatableAnnotations` or
  `AnnotationSupport.findRepeatableAnnotations`.
- Inspect CratonVM `getAnnotations()` / `@Repeatable` container unwrapping for a
  cycle that lacks the visited-guard HotSpot relies on.

## Status
- [x] Reproduced (package `consumer.internals`, `--nojit`); CVM-only.
- [ ] Minimal repro / root cause / fix (open).
