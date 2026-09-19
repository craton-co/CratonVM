# WORKER-3-NOTE-9 — `RJdkJmx`: one gap closed, and the other one is a decision somebody has to take

**2026-08-23**, MEASURED on `af3f6c943`. **`RJdkJmx` is still red.** This note
closes the first of its two causes, gives the second an exact diagnosis and the
commit that introduced it, and does not take a policy decision in another lane's
file at the end of a session.

## 1. Cause one — `JavaLangAccess.layers` was never implemented. FIXED

```text
NoSuchMethodError: java.lang.System$1.layers(Ljava/lang/ClassLoader;)Ljava/util/stream/Stream;
  caller java/util/ServiceLoader$ModuleServicesLookupIterator.iteratorFor @pc=80
```

`javap -c` on JDK 25 gives the contract: `iteratorFor` consults `layers` only
when the loader is **neither null nor the platform loader** — i.e. the
application loader — and then walks the returned layers calling
`providers(layer)`. `ManagementFactory.getPlatformMBeanServer()`'s real bytecode
reaches it during platform-component discovery, so strict mode died there.

Implemented on both `java/lang/System$1` (the concrete receiver `invokeinterface`
dispatches through) and `jdk/internal/access/JavaLangAccess`, returning
`Stream.empty()`.

**Empty was measured, not assumed.** Both candidates were built into one binary
behind a selector and run:

| return | result |
|---|---|
| `Stream.of(ModuleLayer.boot())` | walks into the NEXT unimplemented method, `System$1.getServicesCatalog(Ljava/lang/ModuleLayer;)` |
| `Stream.empty()` | `ManagementFactory` init proceeds past this point |

Empty is also the honest answer for this VM: it defines no **named** modules to
the application class loader, and that loader is the only one for which the JDK
asks.

## 2. Cause two — a refusal that reaches a `<clinit>`. NOT FIXED

With `layers` in place the vector gets further and dies here (frames outermost
first, as this VM prints them):

```text
NoClassDefFoundError: cratonvm/internal/BufferPool
  ManagementFactory.getPlatformMBeanServer(ManagementFactory.java:473)
  DefaultPlatformMBeanProvider$10.nameToMBeanMap(...:413)
  sun/management/ManagementFactoryHelper.getBufferPoolMXBeans(...:341)
  jdk/internal/misc/VM.getBufferPools(VM.java:480)
  jdk/internal/misc/VM$BufferPoolsHolder.<clinit>(VM.java:468)
```

`VM$BufferPoolsHolder.<clinit>` calls `JavaNioAccess.getBufferPool()`, which is
`alloc_buffer_pool` in `shared_secrets_bridge.rs`. Under `--jdk-only`
`try_ensure_synthetic_class` refuses to fabricate the carrier — a blanket policy
in `class_manager`, with no per-class allow-list — and the function deliberately
turns that refusal into a throwable:

> *"Strict mode refused the fabrication… so the refusal stands as a catchable
> throwable rather than a silently empty list."*

**Introduced by `9d3f78943` "fix(jmx,reflect): a 'direct' BufferPoolMXBean that
exists, and counts"**, whose own message records what it replaced: *"fell to the
empty-list arm"*. That arm is why this vector was green before.

### The argument the record needs, and it is one line

**The caller is a `<clinit>`.** A throwable out of a static initialiser is not
"catchable" in any useful sense — it becomes `ExceptionInInitializerError`, then
`NoClassDefFoundError`, and it **poisons `jdk.internal.misc.VM` for the life of
the process**. It does not cost the buffer-pool bean. It costs **the entire
platform MBeanServer** — every MXBean, and all 67 of this vector's checks, nearly
none of which are about buffer pools.

So the premise the refusal-as-throwable rests on is false for its actual caller.
That is the same shape as the lesson written 30 lines above the fabrication
policy in `vm_init.rs`: *"a guard scoped by a premise about who calls you is only
as good as that premise."*

### The three resolutions, and why none is taken here

1. **Exempt the carrier from the jdk-only fabrication policy.** `cratonvm/internal/BufferPool`
   is a private carrier implementing a JDK *interface*; there is no real class it
   could be shadowing, so refusing it does not cause real bytecode to run — it
   causes a crash. Defensible, but it is a change to the contract's central
   lever, in `classloading`.
2. **Let `JavaNioAccess.getBufferPool()` stop intercepting under strict mode**,
   so real `java.nio` bytecode builds the pool and no fabrication is needed. This
   is the contract-correct direction and my preferred one, but it needs the
   direct-buffer accounting question answered.
3. **Revert `9d3f78943`'s list change** — restores green, loses the observability
   it added.

All three are decisions for whoever owns JMX/`classloading`. **I have not taken
one**, and the vector stays red until somebody does.

## 3. Also landed — the pool LIST no longer throws

`alloc_all_buffer_pools` now returns an empty `Vec` when the fabrication is
refused, instead of propagating. That is the answer to
`ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)`, a different
caller from the `<clinit>` above and one where an empty list is both harmless
and what the JDK permits. **It is not exercised by any vector** — stated plainly
rather than implied by the diff — and it does not change §2.

## 4. Verification

| arm | result |
|---|---|
| `CRATONVM_ARGS=--jdk-only` | 106/107 — `RJdkJmx`, i.e. unchanged from control |
| `SUITE=all` | **107/107** |
| `SUITE=core` | **67/67** |

No regression at any arm. `RJdkJmx` is the only failure and is the one this note
does not close.

> **A note on the reproduction.** The first several manual runs were against
> `java.home=/usr/lib/jvm/java-21-openjdk-amd64` — the system default — because
> a bare `cratonvm` does not inherit the suite's `JDK`. The diagnosis was
> re-confirmed with `--java-home /data/toolchain/jdk-25` before any of it was
> written down. Anything reproducing a suite failure by hand needs that flag.

## Index rows for `INDEX.md` (H0 to place)

* `WORKER-3-NOTE-9` §1 — `JavaLangAccess.layers(ClassLoader)` was unimplemented;
  `ServiceLoader` consults it for the APP loader only, and
  `getPlatformMBeanServer` dies there. FIXED, `Stream.empty()`, chosen by
  building both candidates
* `WORKER-3-NOTE-9` §2 — `RJdkJmx`'s remaining cause: a refused fabrication of
  `cratonvm/internal/BufferPool` thrown out of `VM$BufferPoolsHolder.<clinit>`,
  which poisons `jdk.internal.misc.VM` and takes out the WHOLE platform
  MBeanServer. Introduced by `9d3f78943`. Three resolutions listed; none taken
* `WORKER-3-NOTE-9` §4 — a bare `cratonvm` uses the SYSTEM `java.home` (21 here),
  not the suite's `JDK`; pass `--java-home` when reproducing a suite failure
