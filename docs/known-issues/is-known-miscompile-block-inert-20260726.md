# The entire `is_known_miscompile` matches! block (~950 lines, dozens of named bans) is already dead code under every default run

**Status: consolidated finding, no code change (matches existing convention of leaving inert entries in place).**
Part of the "remove all app-specific JIT bans" sweep, 2026-07-26.

## Finding

`vm/src/jit/skip_list.rs`'s `is_known_miscompile(class_name, method_name)`
function (lines ~2142-3096, essentially one giant `matches!(...)` tuple
match) is called from exactly one call site:

```rust
if callee_saved_gpr_local_homes_enabled()
    && is_known_miscompile(class_name, method_name)
    && !package_allowed(class_name, allow_packages)
{
    return Some(...);
}
```

`callee_saved_gpr_local_homes_enabled()` defaults to `false` on x86_64
(only becomes `true` if a developer explicitly sets
`CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=1` — see that function's own
doc comment, ~line 3325 in the pre-this-session file). Rust short-circuits
`&&`, so **`is_known_miscompile` is never even invoked** in any default
build/run, and every one of its dozens of match arms is unreachable dead
code today.

This was already independently confirmed for two individual entries this
session (BC-ASN1.1 / `java/util/Calendar.isFieldSet`, FELIX.1 /
`AccessibleObject.setAccessible`+`SecureAction.lambda$getAccessor$0`) before
realizing the *entire* function shares this fate. A non-exhaustive list of
other named historical bans living inside this same dead function (there
are more — this function is ~950 lines):

- **EXEC.1** — `ThreadPoolExecutor.{execute,runWorker,getTask}`,
  `LinkedBlockingQueue.{offer,enqueue,take,dequeue}`,
  `AtomicInteger.{incrementAndGet,getAndIncrement}`,
  `CountDownLatch.{countDown,await}`,
  `CountDownLatch$Sync.{tryReleaseShared,tryAcquireShared}`
- **W2-CHM** — `Integer.<init>`, `Long.<init>` (redundant with the generic
  non-trivial-constructor gate anyway, per that entry's own comment)
- **RBC.1** — `String.{toLowerCase,toUpperCase}`,
  `Provider$ServiceKey.{hashCode,equals}`,
  `Provider.{put,parseLegacy,putService,implPut}`
- **SPB.1** — `HashMap.{put,get,resize,putVal,newNode,treeifyBin,hash,
  afterNodeInsertion,afterNodeAccess,afterNodeRemoval}`,
  `LinkedHashMap.{newNode,newTreeNode,afterNodeInsertion,afterNodeAccess,
  afterNodeRemoval}`, `String.hashCode`
- **SPB.8** — `Long.parseLong`, `Integer.parseInt`
- **SPB.2** — `Class.getGenericInterfaces()`-adjacent entries
- **SPB.3** — `SourcesIterator.{fetchNext,hasNext,next}`
- **HIB-PROXY** — ByteBuddy lazy-proxy generation entries
- **KC26.LR** — `io/smallrye/config/ConfigValueConfigSource$ConfigValueProperties$LineReader.readLine`,
  `...ConfigValueProperties.load0`
- **KC-CRED.LAZY** — `PasswordCredentialData`/`PasswordSecretData.getAdditionalParameters`
- **ES-HANG-01** — `WeakHashMap$*Spliterator.{tryAdvance,forEachRemaining}`
- **NETTY.1** — the historical `Arrays.fill` entry, already independently
  known-lifted (comment says "NETTY.1 LIFTED (2026-06-11...)")
- **JUNIT.1** — `JUnitCore.main` (this session removed the separate,
  ACTIVE copy of this ban that lived outside this gated function — see
  the main removal commit; this was a second, already-dead duplicate
  entry inside `is_known_miscompile` for the same class/method)
- **BC-ASN1.1** — `Calendar.isFieldSet`
- **FELIX.1** — `AccessibleObject.setAccessible`, `SecureAction.lambda$getAccessor$0`
- Several more `ecj`/Eclipse-JDT `HashtableOfInt.rehash`-family and
  Keycloak credential entries in the same block, not individually
  catalogued here.

## Why no code change was made

Leaving these entries in place (rather than deleting them) matches this
file's own established convention for confirmed-dead bans — e.g. the
"NETTY.1 LIFTED" and prior "REMOVED, kept as dead enum variant" notes
already in this file treat a confirmed-inert entry as safe to leave for
historical/diagnostic value, only actually deleting code when a *specific*
ban was re-tested and found safe under the CLI-reachable (Conservative)
policy. `is_known_miscompile`'s entries are reachable only via
`SkipPolicy::Aggressive` combined with the callee-saved-GPR flag — a
combination requiring TWO non-default settings, neither of which is wired
to the CLI (`jit_aggressive_compilation` has no CLI/env flag either, per
`docs/internal/jit-ban-sweep-20260725.md`). Since nothing in a real,
default `cratonvm` invocation can ever reach this code, there is no
behavior to change by deleting it, and doing so would be pure code
deletion with no test to write against (the existing
`known_miscompile_overrides_apply_to_actual_package` and related unit
tests already assert this function's behavior in isolation, which remains
correct and unchanged).

## What this means for the broader "remove all app-specific JIT bans" effort

This single finding resolves the individual-real-app-verification question
for roughly 20-30 of the ~46+ originally-catalogued bans at once: none of
them need a dedicated fixture-app re-test, because none of them currently
restrict anything. Future sessions surveying the remaining backlog should
first check whether a candidate ban lives inside `is_known_miscompile`
(search for its class/method tuple inside that function's `matches!` body,
lines ~2142-3096) before spending fixture-discovery effort on it — if so,
it is already resolved by this finding and needs no further work.

**Caveat:** this does NOT mean the underlying miscompiles these entries
describe are fixed. If `callee_saved_gpr_local_homes_enabled()`'s default
is ever flipped to `true`, or `jit_aggressive_compilation` ever gets real
CLI wiring, every one of these entries becomes reachable again and would
need to be individually re-verified before considering it safe. This
finding is about *current default reachability*, not about root-causing
or fixing the historical bugs themselves.
