# Untriaged oddities — 2 classes

Not confirmed as fixture gaps or CratonVM bugs — flagged for a future
session to spend ~15-30 minutes reading the failing test method before
categorizing further. Both fail identically on real JDK 25 (HotSpot) in the
same fixture, so neither is (so far) evidence of a CratonVM-specific defect,
but neither has an obvious environment-gap explanation the way the other
known-issues docs in this folder do.

## `org.apache.catalina.startup.TestTomcat`

```
org.apache.catalina.LifecycleException: Deliberately Broken
Deliberately Broken
```

The string "Deliberately Broken" strongly suggests this is the test
**exercising** an intentionally-broken-webapp scenario (verifying Tomcat
handles a deliberately-misconfigured deployment gracefully) rather than the
fixture itself being broken. Read `TestTomcat.java`'s failing method first —
this may turn out to be a legitimate test-framework artifact of this
one-process-per-class harness (e.g. the test expects to catch the exception
itself and continue, but the whole JVM's JUnit run reports it as a class
failure because of how logging/stderr is captured) rather than anything
needing a fixture fix.

## `org.apache.jasper.compiler.TestNonstandardTagPerformance`

```
java.lang.ClassNotFoundException: org.apache.jasper.compiler.TestNonstandardTagPerformance
```

Self-referential — the class fails to find *itself*. Smells like a
classloader/classpath-ordering quirk specific to this one class (maybe a
duplicate/shadowed class on the flat classpath, or a JSP-compiler test that
dynamically reloads its own test class via a different classloader and gets
the ordering wrong). Not understood yet; needs someone to actually read
`TestNonstandardTagPerformance.java` and reproduce standalone with
`-verbose:class` to see which classloader is asking for it and why the
lookup fails.
