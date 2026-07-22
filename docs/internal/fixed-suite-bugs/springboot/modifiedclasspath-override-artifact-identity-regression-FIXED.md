# Modified classpath override artifact identity

**Status: FIXED - 2026-07-18**

`ModifiedClassPathExtensionOverridesTests` and its parameterized counterpart
put Spring Framework 4.1.0 artifacts ahead of the normal Spring Boot test
classpath. Their nested `ModifiedClassPathClassLoader` must expose
`ApplicationContext` and `StringUtils` as classes defined by those override
JARs, including their `ProtectionDomain` / `CodeSource` identity.

## Root cause

The isolated URL loader correctly found and defined the local 4.1.0 bytes, and
the loader-faithful constant-pool resolver correctly selected that local class
identity. `Class.getProtectionDomain()`, however, discarded the mirror's exact
`ClassId` and called the loader-blind `find_class_source_path(name)` helper.
For a same-named application class, that helper returned Spring 7.0.7, so the
reported `CodeSource` was wrong despite the class itself being loader-local.

The same investigation found a residual resource leak: a later parent-first
`URLClassLoader.getResource` repair was also applied to intentionally isolated
modified-classpath loaders. It could reintroduce resources that an exclusion
had removed.

## Fix

- Local URL-loader definitions now keep the code-source URL and signer data
  from the exact constructor classpath entry that supplied their bytes.
- `Class.getProtectionDomain()` now reads that exact mirror `ClassId`'s
  `CodeSource`; it uses the legacy name lookup only when old metadata is absent.
- Isolated URL loaders skip the parent resource fallback and retain their local
  exclusion boundary. Ordinary URLClassLoader subclasses remain parent-first.

## Validation

Using JDK 25.0.3 and
`C:\craton\CratonVM-target-modifiedclasspath-identity-20260718-019f753a\release\cratonvm-modifiedclasspath-identity-20260718-019f753a.exe`:

- Focused modified-classpath set: 5/5 PASS with JIT and 5/5 PASS with `--nojit`.
  It covers override, parameterized override, exclusion, fork, and parameterized
  fork classes.
- Combined HTTP-client/classpath closure: 8/8 PASS with JIT and 8/8 PASS with
  `--nojit`, including the three prior HTTP-client classpath-presence classes.

Results are in
`apps/spring-boot-suite-runner/.suite-modifiedclasspath-identity-20260718-019f753a/results/`
under runs `modifiedclasspath-identity-20260718-019f753a-r2` and `-r3`.
