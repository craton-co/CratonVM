# Probes for the 2026-09-10 type-variable scope-walk defect

Ten single-file probes used to take
`the-type-variable-scope-walk-never-climbed-its-loop-body-shadowed-the-binding-20260910`
from a Spring suite row to one line of Rust. Each is plain Java, no build
system: compile against the Spring suite runner's own classpath dump and run
the same command under `java` and under `cratonvm`.

```bash
CP=$(sed -n '2p' <suite-out-dir>/.af_spring-test.txt)   # Gradle's testRuntimeClasspath
javac -nowarn -proc:none -d . -cp "$CP" *.java
java                              -cp ".:$CP" SoftProbe
cratonvm --java-home <jdk25> -cp ".:$CP" SoftProbe | grep -v '^\[cratonvm\]'
```

`.af_spring-core.txt` is the equivalent dump for the `spring-core` module —
`KStack` on `core.BridgeMethodResolverTests` needs that one.

| probe | question it answers | verdict it gave |
|---|---|---|
| `SoftProbe` | does one AssertJ `assertSoftly` survive? | the minimal repro: `SOFT_OK` on HotSpot, `Could not create type` on CratonVM |
| `TvDecl` | what is `getGenericDeclaration()` for each type variable in `AbstractObjectAssert.returns`? | **the root cause**: `SELF`/`ACTUAL` said "the method", HotSpot says "the class" |
| `PoolProbe` | does ByteBuddy's class-file path (`TypePool`) agree with its reflection path (`ForLoadedType`)? | `POOL` identical on both VMs, `LOADED` diverges — names core reflection, and kills the `getDeclaredMethods()`-order hypothesis |
| `GraphProbe` | what does ByteBuddy's `MethodGraph` elect for `returns`? | the divergence in ByteBuddy's own terms; identical to HotSpot after the fix |
| `BbChain` | does ByteBuddy resolve the generic superclass chain (`SELF`→`IntegerAssert`, `ACTUAL`→`Integer`)? | identical on both VMs before *and* after — rules out the substitution machinery |
| `GenChain` | the raw `getTypeParameters()` / `getGenericSuperclass()` tree | identical apart from the minted carrier class |
| `MethodDump` | the full declared-method table, sorted | **sets identical** — no missing or extra bridge |
| `OrderDump` | the same table, unsorted | the order difference; `PoolProbe` then showed order is not the cause |
| `BbProbe` | does a plain `new ByteBuddy().subclass(k).make()` work? | yes on both — the failure needs AssertJ's fuller builder |
| `KStack` | run a JUnit class printing full stack traces and causes | turns a suite `FAILCAUSE` one-liner into the `Cannot resolve ACTUAL` chain |

`KStack` is generally useful beyond this defect: the suite runner's
`failcauses.log` records only an exception's type and message, and for an
`AssertionFailedError` that message is often empty.
