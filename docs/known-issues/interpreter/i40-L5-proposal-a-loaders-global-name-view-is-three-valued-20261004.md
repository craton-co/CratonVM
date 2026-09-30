# Proposal: a loader's view of the JDK-global names is three-valued, recorded once, read by the interpreter and the JIT

**Status: proposal — filed 2026-10-04 by interpreter round i1 wave 40, lane
L5. Not implemented.**

## The problem

Two consumers ask "does this user loader answer a JDK-global name
(`javax/`, `jdk/`, `sun/`, `com/sun/`) exactly as CratonVM's global route
does?":

* the interpreter, `constants.rs` `drive_loader_for_global_name`, before it
  skips asking the loader;
* the JIT, `jit_bridge.rs` `jit_user_loader_global_name_change` /
  `loader_faithful_global_name_answer`, before it binds the built-in class at
  a cold site.

Both read one `bool` per loader, `ClassRealm::loader_global_name_transparency`,
computed by `classloader_real::loader_answers_jdk_names_as_the_jdk` (no
`loadClass` override on the chain). Wave 40 (lane L5) found the answer is not
a property of the loader alone: a transparent loader whose chain ends at the
bootstrap or platform loader answers the RUNTIME-IMAGE names as the global
route does, but not a `javax/inject`-style name on the application class
path, which only a chain reaching the application loader sees. The
interpreter now asks per name
(`classloader_real::transparent_loader_answers_as_the_global_route`, a
parent walk plus a runtime-image lookup on each such resolution miss). The
JIT cannot: it has no `NativeContext`, so it still reads `true` as "bind the
built-in class" and a cold compiled site of such a loader binds the
application's class (`i26-L5` Progress (wave 40)). And the map is filled only
when the interpreter first asks, so the JIT sees `transparency-unknown` for
every loader the interpreter has not classified yet, which defers sites that
need not be (`i37-L2-proposal-classify-a-loaders-jdk-name-transparency-when-it-first-defines`).

## The direction

Record, once per loader, at its first define (where the loader object and a
`NativeContext` are in hand, as the `i37-L2` proposal suggests), a
three-valued view:

* `AppChain` — no `loadClass` override on the chain, and the chain reaches
  the application loader: every JDK-global name is answered as the global
  route answers it;
* `ImageOnly` — no override, the chain ends at the bootstrap or platform
  loader: runtime-image and bootstrap-appended names as the global route,
  every other name through the loader;
* `Opaque` — an override somewhere: ask the loader.

Both consumers read the same per-VM record (`ClassRealm`). The name half of
`ImageOnly` is the runtime-image question that
`NativeContext::runtime_image_defines_class` answers today; the JIT needs
the same answer without a thread (the implementer should check which
per-VM index backs that native and expose it read-only). The interpreter's
per-miss parent walk then disappears, and the JIT's cold-site rule becomes
exact.

## Cost and risk

Cold: one classification per loader at its first define (a parent walk, a
declared-methods scan), then a map read where one happens today. The risk
is behavioural only for `ImageOnly` loaders at JIT cold sites (they defer a
class-path name instead of binding the application's class); measure the
deferred-site count on Spring Boot and Tomcat with
`CRATONVM_DBG_ISOLATED_CNF=1` as wave 37 did. `--compatible` is not
affected (neither consumer runs there).

## Progress (wave 41) — lane L5: carries the i26-L5 JIT cold-site remainder

`i26-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class`
was closed by wave 41 (lane L5) into
`docs/internal/fixed-bugs/interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md`;
its JIT cold-site item is this proposal's "the JIT cannot" paragraph above.
The evidence (from the code, as i26-L5 "Progress (wave 40)" traced it):
`jit_bridge.rs` `jit_user_loader_global_name_change` reads
`loader_global_name_transparency == true` as "the built-in answer stands"
for every JDK-global name, so a cold compiled `new javax/l5w40/Lazy` in a
class of a class-path-blind loader (no `loadClass` override, null or
platform parent) binds the APPLICATION loader's copy until the interpreter
has run that site; the interpreter asks the loader since wave 40
(`L5W40BootParentGlobalName`). A probe for the implementer: that probe's
reference moved into a cold branch of a method compiled first (the
`L2W37JdkNameColdSite` shape).

## Progress (wave 46) — lane L5: carries the i29-L5 JIT cold-site item

Moved here from
`docs/internal/fixed-bugs/interpreter-L5-loader-constraints-are-not-imposed-at-member-resolution-FIXED-20261010.md`
(closed in wave 46): a compiled cold site of a user loader's class that binds
a JDK-global name without asking the loader also binds it without the
loader-constraint check the interpreter's door makes
(`check_initiating_load`); the three-valued, recorded view this proposal
describes is what would let the JIT ask. No code change in wave 46.
