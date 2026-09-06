# Two NPEs in Spring Boot's own jar/nested URL-protocol handlers — two unrelated defects, one fixed, one split out

Status: **FIXED for row 1 and for the blocker that had replaced row 2.**
`JarUrlConnectionTests` is 47/47 and `NestedUrlConnectionTests` 11/11 on
CratonVM with the JIT on and off, and both pass on HotSpot on the same
harness. Row 2 leaves a residual that is **not** fixed and now has its own
page — see the end.

The page this replaces grouped the two rows on shape alone and said so:
*"could equally be two independent gaps."* They are, and neither guess about
the shared cause was right.

## Row 1 — `JarUrlConnectionTests`: a `spy()` earlier in the class dropped `JarFile`'s native shadow

```
java.lang.NullPointerException: Cannot read field "zsrc" because "<local3>.res" is null
	at java.util.zip.ZipFile.getInputStream(ZipFile.java:327)
	at java.util.jar.JarFile.getInputStream(JarFile.java:834)
	at …JarUrlConnection.deduceContentTypeFromStream(JarUrlConnection.java:144)
```

The page's own next step — *"reproduce each in isolation … to rule out
suite-ordering effects"* — was the whole answer.
`getContentTypeWhenNotKnownInStreamButKnownNameReturnsDeducedType` **passes
alone** and fails in the class. Two tests are enough:

```
getInputStreamWhenNoCachedClosesJarFileOnClose                    PASS
getContentTypeWhenNotKnownInStreamButKnownNameReturnsDeducedType  FAIL (the NPE)
```

and the first is the only test in the class that calls `spy(jarFile)`. Three
other predecessors that call `mock(URLConnection.class)` or
`mock(JarUrlConnection.class)` do NOT arm it: the mock has to be of the
`JarFile` itself.

**Mechanism.** Mockito's inline mock maker instruments its target's whole
superclass chain, so `spy()` on Spring's `UrlJarFile` retransforms
`java.util.jar.JarFile` and `java.util.zip.ZipFile`. CratonVM's
suppress-native-shadow-on-redefine rule then drops the registered natives for
those two classes so an agent's woven bytecode can run — right for an ordinary
class, and catastrophic here: a CratonVM `JarFile` keeps its archive in a Rust
handle table (`jar_table()`, `native-io/src/zip_real_jar.rs`), not in the JDK's
`CleanableResource res` / `Source zsrc` field graph. The real body can only
NPE, for every jar in the process, from that point on.

**Fix.** `redefine_immune_zip_file_native` in
`vm/src/runtime/interpreter/native_override.rs`, added to BOTH aggregators
(`redefine_immune_layout_native` for the invoke-cache paths and
`redefine_immune_forced_native` for the slow path — an arm added to one only is
the failure the collections entry already made once).

This is the third member of a family the file already documents: the
synthetic-collection arm (a `mock()` anywhere emptied every `HashMap`) and the
`ThreadLocal` arm (a `mock()` of `NamedThreadLocal` emptied every
`ThreadLocal`). Same rule each time: **a class whose CratonVM instances have no
real JDK field graph cannot have its native shadow dropped by a
redefinition.** The immunity is method-wise and keyed to the SAME method sets
the two force-native gates already use, so a `JarFile` method CratonVM does not
claim stays evictable and an agent can still weave it.

## Row 2's blocker — a JIT register-residency bug, in a method with no jars in it

By the time this was investigated, `NestedUrlConnectionTests` was not failing
with the NPE the original page recorded. It was failing inside
`mock(NestedUrlConnection.class)`, 100% of the time with the JIT on:

```
org.mockito.exceptions.base.MockitoException: Mockito cannot mock this class …
  Caused by: java.lang.IllegalStateException: Byte Buddy could not instrument
    all classes within the mock's type hierarchy
  Caused by: java.lang.ArrayIndexOutOfBoundsException: Index 15029 out of bounds for length 5
	at net.bytebuddy.jar.asm.ClassReader.readCode(ClassReader.java:2056)
```

Bisected to one switch, `CRATONVM_JIT_IR_PHI_COPY_REGS`, one of the thirteen
optimizing-tier defaults flipped ON earlier the same day (`a740427a`), and
then with `CRATONVM_JIT_DENY` to two compiled bodies: denying
`net/bytebuddy/jar/asm/ClassReader.readCode` AND
`net/bytebuddy/jar/asm/Label.resolve` together is the minimal set that makes
the class pass; either alone still fails.

**Root cause.** `gp_reg_live` was monotone. The residency plan hands ONE
physical register to any number of values whose live ranges do not overlap
(`plan_slots` demotes only pairs whose ranges DO overlap), so `gp_reg_of` is
many-to-one — but `mark_gp_reg_live` only ever set bits and never cleared one.
Once a second value published into a shared register, `resident_gpr` still
answered "yes, in register R" for the FIRST value, and every later read of it
took whatever the second value had put there.

Nothing read a value far enough from its own definition for that to matter
until the phi-copy edge read arrived: `emit_copy_op`'s
`ir_phi_copy_regs_enabled()` branch asks `resident_gpr(source)` at the END of a
predecessor block, which is exactly where a register can have changed hands
since the source was published. One switch appeared to be the bug and was
really only the first reader of a latent one.

**Fix.** `gp_reg_owner: [Option<NodeId>; 16]` in `jit/src/ir_lower.rs`:
`mark_gp_reg_live` evicts the previous occupant of the register it is about to
write. Every GP register write goes through that one function, so the
invariant `resident_gpr` actually needs — *this node is the CURRENT occupant of
its register* — now holds. The optimizing-tier defaults stay ON; nothing was
reverted. Two lowering tests cover it, and both fail without the eviction.

**Blast radius, stated plainly.** This was a wrong-code regression on dev, and
`NestedUrlConnectionTests` is where it happened to be caught.
`JarUrlConnectionTests` was hit by it too (16 Mockito failures on top of its
own NPE), and anything that mocks inline through Byte Buddy was exposed.

## Evidence

Binary: release build of dev `d2cfbd543` plus these fixes, `cratonvm-psl4`,
Azure host 2. Suite runner: `apps/spring-boot-suite-runner`.

| class | before, JIT on | after, JIT on | after, `--nojit` | HotSpot |
|---|---|---|---|---|
| `JarUrlConnectionTests` | FAIL (16 + the NPE) | **PASS 47/47** | PASS | PASS |
| `NestedUrlConnectionTests` | FAIL (every `mock()`) | **PASS 11/11** | PASS | PASS |

The HotSpot arm is the cross-check the original page listed as missing: both
pass on the reference VM, so both rows were genuine CratonVM defects.

## The row that did not retire with this page — and then did (2026-09-06)

With the two fixes above in place, the ORIGINAL 2026-09-04 symptom of row 2 —
`"this.resources" is null` out of `NestedUrlConnection.connect` — came back at
about 1 run in 40 with the JIT on, under load. It was split out rather than
held here, because rows 1 and 2 were never one defect.

It is now fixed too, and it was a THIRD unrelated defect: a moving collector
relocated the `Class[]` that `Instrumentation.retransformClasses` was walking,
the unpinned array read a zero word out of the vacated address, and the rest of
Mockito's superclass chain was silently never instrumented — so the FIRST call
on the mock ran the real body. See the retired
`retransform-class-array-relocated-mid-loop-half-woven-hierarchy-FIXED-20260906`
write-up, which also corrects this page's reading of the stack: the throwing
line is the STUBBING call, not the assertion, so it was never a self-call
problem at all.

Three rows, three unrelated root causes, in one pair of test classes. The
grouping this page inherited was wrong in every direction it could be.

## Repro (for a future regression of what IS fixed here)

```bash
cd apps/spring-boot-suite-runner
CV_BIN=<binary> pwsh -File ./run-spring-boot-suite.ps1 -Category all \
  -ClassList <(printf 'module\tclass\n%s\t%s\n%s\t%s\n' \
    loader/spring-boot-loader org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests \
    loader/spring-boot-loader org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests)
```

Row 1 needs the whole class (or the two-test pair above) — the failing test
passes alone. The Byte Buddy blocker needs `-Jit on`;
`CRATONVM_JIT_IR_PHI_COPY_REGS=0` is the switch that used to hide it.
