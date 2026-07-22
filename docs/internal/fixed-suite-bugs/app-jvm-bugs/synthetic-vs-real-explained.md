# Synthetic stubs, shims & intrinsics — explained simply

*Audience: anyone who wants to understand "what is all this Rust code that sits
on top of a real JVM, and do we actually need it?" No Rust knowledge required.*

---

## The one-sentence version

CratonVM is a **real JVM**: it reads actual Java `.class` files and runs their
bytecode, just like HotSpot does. On top of that real engine sits a layer of
Rust code (the "native overlay"). That overlay is **three different things
wearing the same uniform** — and only one of them is a problem.

---

## How a method call is decided

When Java bytecode calls a method, CratonVM picks who handles it in this order
(see `invoke_or_native` in [vm/src/vm/vm_exec.rs](../vm/src/vm/vm_exec.rs)):

1. Is there a **fast Rust path** registered for this exact method? → use it.
2. Otherwise → **run the real Java bytecode** from the `.class` file.
3. If neither exists → error.

Step 1 is the overlay. The catch: **a registered Rust method wins even when the
real Java bytecode also exists and would be correct.** That's the whole story of
why the overlay can be dangerous.

---

## The three kinds of overlay code

| Kind | What it is | Why it exists | Real answer? | Verdict |
|------|-----------|---------------|--------------|---------|
| **Intrinsic** | A hand-written fast version of a hot method (e.g. `Math.abs`, `String.length`, `System.arraycopy`). | Speed. The real bytecode would give the same answer, just slower. | ✅ Identical to real | **KEEP** |
| **Bridge** | A native the VM *needs* because it genuinely can't run the real thing: OS calls (sockets, files), `sun.*` internals that depend on JVM state we set up ourselves, or classes that have **no** real bytecode at all. | Necessity. There is no Java bytecode to run instead. | ✅ It *is* the real behavior | **KEEP** |
| **Synthetic stub** | A **fake**. Returns a placeholder, an approximate, or an outright **wrong** value; fabricates objects (e.g. fake crypto keys); or short-circuits an app's `main()` so the program "exits successfully" without actually running. | A shortcut from early development to make something *look* like it works before the real path existed. | ❌ Often wrong | **REMOVE** |

Concrete examples in this tree:

- **Intrinsics** live in [native-builtins/src/intrinsics/](../native-builtins/src/intrinsics) — `Math.*`, `String.length`, `StringBuilder.append`, etc.
- **Bridges** — the socket/file syscalls, the JBoss module loader (`jboss_module_loader`), `sun.*` shared-secrets shims.
- **Synthetic stubs** — the `*_extras.rs` "fake main" launcher shims (Jetty, SonarQube, Liberty… that *"short-circuit the launcher's `main` so the JVM exits rc=0 without actually running the server"*), the synthetic crypto keys in [crypto.rs](../native-builtins/src/crypto.rs), and a handful of "intrinsics" that return the wrong constant (e.g. `Collections.disjoint`, `Collectors.toMap` duplicate-merge, `String.format("%s", …)` on boxed values).

---

## Why a synthetic stub is the dangerous one

Because of the dispatch order above, a stub **silently shadows correct Java
bytecode**. The real `.class` is sitting right there, able to produce the right
answer — but the fake runs first and returns something wrong, with **no warning**.
A test passes, an app "boots," a number comes back… and it's quietly incorrect.

That is worse than a crash. A crash you notice and fix. A wrong answer you ship.

The most extreme case found in this codebase: the "fake main" shims were wired
into the **default** startup path, so launching certain servers would report
success (`exit 0`) without ever running the server. The app *looked* like it
passed.

---

## The rule (this is the policy now)

> **Real bytecode → run it. Can't run the real thing → throw a clear, descriptive
> error that names the exact `class.method descriptor`. Never return a fake.**

Keep **bridges** (they *are* the real behavior). Keep **intrinsics** *only* where
a differential test proves they match the real bytecode exactly. Delete or
disable **synthetic stubs** — and when removing one leaves a genuine gap, fail
loudly with a clear message instead of faking a result.

A clear error is a to-do item. A wrong answer is a bug you can't see.

This formalizes the older policy in
[docs/jvm-no-synthetic-stubs.md](jvm-no-synthetic-stubs.md): real `.class` bytes
take precedence; natives may *bridge* a real class, but must never *replace* it
with a parallel fake type.

---

## How we make this safe (the safety net being built)

1. **Tag** every native as intrinsic / bridge / synthetic-stub, and dump a
   census so we can see exactly what's what.
2. **Differential test**: run a method both ways — Rust native vs the real Java
   bytecode — and compare. Agreement licenses "keep"; divergence flags a fake.
3. **Remove**: delete stubs the real bytecode already covers; move the rest
   (synthetic crypto, app unblockers) behind a default-OFF feature flag so the
   normal build is fake-free.
4. **Clear error**: when nothing real can run, throw a descriptive
   "unimplemented" exception naming the method — never a placeholder value
   (`CRATONVM_TRACE_UNIMPLEMENTED=1` prints each one).
5. **Remove them all at once**: `CRATONVM_NO_STUBS=1` puts the registry in strict
   mode — every synthetic-stub registration is dropped, so calls hit real
   bytecode or a clear error, never a fake. Opt-in (off by default) because some
   apps still limp on the fakes. Per-class: `CRATONVM_REAL=<class>|all|jca`.
6. **CI gate**: fail the build if a new synthetic stub sneaks back into the
   default path.

---

## Where are we on "run any Java app"?

- **Real bytecode is the default engine.** CratonVM loads `java.base` and app
  classes from a real JDK on disk and interprets/JITs them. (See
  [README.md](../README.md).)
- **Synthetic-JDK mode is an opt-in fallback** (`--synthetic-jdk`) for running
  with *no* JDK present — not the default.
- **The gauntlet** ([apps/TARGET_APPS.md](../apps/TARGET_APPS.md)) is 40+ real
  apps (WildFly, Kafka, Spring, Keycloak, Cassandra…) that must start and pass
  end-to-end with no fakes. Some currently "pass" only because a stub
  short-circuited them — **those will now fail loudly**, which is the point: a
  visible gap we can fix, instead of a hidden lie.
- **Trajectory:** the project has been moving from "synthetic everywhere" toward
  "real bytecode + targeted bridges." This work finishes that turn for the
  stub layer.

**Do we need the overlay?** Yes — the bridges and the proven intrinsics. We do
**not** need the synthetic stubs, and keeping them costs us correctness. This
plan keeps the useful two-thirds and removes the dangerous third.
