# MockResolver / forked-classloader isolation probes

All four probes here **pass** on CratonVM (matching HotSpot) — kept as a
starting point for a future session, NOT as failing repros. See
`docs/known-issues/springboot/mockresolver-dynamicclassloader-classnotfound-forked-testcontext.md`
("session 2") for what each one ruled out.

Package-private access to `CompileWithForkedClassLoaderClassLoader` requires
these to live in the real `org.springframework.core.test.tools` package —
keep the directory structure when compiling/running.

```
javac -cp .;<module cratonvm-test-cp.txt content> org/springframework/core/test/tools/ForkProbe*.java
cratonvm --java-home <jdk25> -c .;<same cp> org.springframework.core.test.tools.ForkProbe
cratonvm --java-home <jdk25> -c .;<same cp> org.springframework.core.test.tools.ForkProbe2
cratonvm --java-home <jdk25> -c .;<same cp> org.springframework.core.test.tools.ForkProbe3 [rounds] [threads]
cratonvm --java-home <jdk25> -c .;<same cp> org.springframework.core.test.tools.ForkProbe4
```

- `ForkProbe`: `getResourceAsStream`/`loadClass`/`getResources` for
  `SpringMockResolver` directly on the app loader, the bare forked
  `CompileWithForkedClassLoaderClassLoader`, and the forked loader as thread
  context loader.
- `ForkProbe2`: the same probes, but INSIDE a real
  `TestCompiler.forSystem().compile(...)` callback — context classloader is
  a genuine `DynamicClassLoader` wrapping the forked loader, matching the
  real failing stack trace's frame exactly.
- `ForkProbe3`: concurrency stress — N threads racing `loadClass` on a fresh
  forked loader, many rounds.
- `ForkProbe4`: pre-loads `org.mockito.Mockito` via the app loader BEFORE
  forking, then creates a real mock inside the fork (tests whether Mockito's
  inline-mock-maker retransform disrupts things once it has already touched
  a class — see `reference_mockito_inline_mock_maker_redefinition_shadow_native`
  in project memory).

A future session chasing this bug should build on these (e.g. add GC
pressure, run under the ACTUAL dual-web-server-context load the real test
has) rather than starting from scratch — the basic mechanism is confirmed
NOT broken in isolation.
