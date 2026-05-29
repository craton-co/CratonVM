# Tomcat NIO Selector — open investigation

## Status

**Open.** After `cdcf159` (real-JDK `InetSocketAddress` holder layout
in NIO bind), Tomcat 10.1.31 reaches `Server startup in [156-361] ms`
with the server socket successfully bound to port 8080. However, the
NioEndpoint poller's selector loop fires continuously with:

```
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error in selector loop (java/io/IOException: ClosedSelectorException)
```

Until this is resolved Tomcat is up but does not accept connections.

## Diagnosis

Added `CRATONVM_DBG_SEL=1` env-gated `eprintln` instrumentation to **all
three** of CratonVM's `java/nio/channels/Selector.open()` native
registrations:

- `servlet.rs::register_s2_selector` (5-field synthetic, S2SEL layout)
- `phases_late.rs::register_p98_*` (4-field synthetic, slot-indexed)
- `tests_extracted.rs::register_*` (5-field synthetic, S2SEL layout)

**During a 25-35 s Tomcat boot, ZERO of these traces fire.** Tomcat's
poller is creating its `Selector` instance through a code path that
*does not* dispatch through our static `Selector.open()` native.

The likely chain (consistent with what bypassed our `RandomAccessFile`
natives during the dacapo-luindex investigation):

```
Tomcat NioEndpoint.Poller.<init>:
    selector = Selector.open();
        ↓
    bytecode of java/nio/channels/Selector.open():
        return SelectorProvider.provider().openSelector();
            ↓
        SelectorProvider.provider():
            (real-JDK static — singleton; on Windows returns
             sun.nio.ch.WindowsSelectorProvider)
            ↓
        WindowsSelectorProvider.openSelector():
            return new WindowsSelectorImpl(this);
                ↓
            WindowsSelectorImpl.<init>:
                wakeupSourceFd = native pipe()  ← throws here, or
                ... = native call ...           ← here, with bogus state
```

The resulting `WindowsSelectorImpl` instance carries CratonVM-allocated
slots that look right (its `<init>` and field setters all run), but the
underlying native pipe / Windows-specific fd handles never get
established because their `WindowsSelectorImpl.poll0` / `Net.pipe0` /
similar natives are not registered. The poller's later
`selector.select(timeout)` enters `WindowsSelectorImpl.doSelect` which
checks `if (closed) throw new ClosedSelectorException()` — the
"closed" bit is set by `AbstractSelector.<clinit>` or by an earlier
swallowed `IOException` in the constructor, so every subsequent
`select` throws the same exception.

Without `[SEL]` traces firing, we can't yet pinpoint which native call
returns the bad state — but the symptom (continuous
`ClosedSelectorException` from a selector our `open()` native never
created) is conclusive.

## Re-application paths, in increasing order of work

1. **Intercept `SelectorProvider.provider()`.** Return a synthetic
   `SelectorProvider` whose `openSelector()` calls our existing
   synthetic Selector path. ~30 lines, plus matching synthetic class
   layout for `SelectorProvider`. Risk: anything else that expects to
   see the real provider (e.g. `inheritedChannel()`,
   `openDatagramChannel()`) breaks unless we cover those methods too.

2. **Make `Selector.open()` static native always win.** The native is
   registered but doesn't fire — investigate why. Possible causes:
   - `java/nio/channels/Selector` is loaded with a different class
     loader than the one our registration is keyed on.
   - The bytecode for `Selector.open()` is short enough that the
     interpreter inlines it as `SelectorProvider.provider().openSelector()`
     without consulting the native registry.
   - There's an `<clinit>`-time intercept that runs before our
     registration phase.
   Confirming requires adding a trace to the native dispatch table
   lookup itself, not just to the registered native.

3. **Implement the real-JDK `WindowsSelectorImpl` / `WEPollSelectorImpl`
   native surface.** `poll0`, `setupPipe0`, `interrupt0`, `wakeup0` plus
   the platform-conditional `getAcceptCount`/etc. This is the path
   HotSpot and OpenJDK take; it's the most correct but the most work
   (estimate: ~500 lines of native registrations plus a fd registry
   for the wakeup pipes).

## What stays in tree after this investigation

- `CRATONVM_DBG_SEL=1` env-gated traces in `servlet.rs` and
  `phases_late.rs` Selector natives. Costs one `var_os` check per
  call when the env var is unset; the steady-state cost is zero.
- This doc.

The actual fix for the selector loop is **out of scope for this
session** — it needs the dispatch-layer investigation in path (2) above
before a path is chosen.

## How to reproduce

```
TOMCAT=C:/craton/CratonVM/test-infra/regression-pool/apps/apache-tomcat-10.1.31
cd $TOMCAT
CRATONVM_DBG_SEL=1 timeout 30 \
    C:/craton/CratonVM/target/release/cratonvm.exe \
    --java-home "C:/Program Files/Java/jdk-25" -Xmx512m \
    -Dcatalina.home=$(pwd) -Dcatalina.base=$(pwd) \
    -cp "bin/bootstrap.jar;bin/tomcat-juli.jar" \
    org.apache.catalina.startup.Bootstrap 2>&1 | tail -30
```

Expected: `Server startup in [...] ms` line, then `ERROR [Acceptor]
Socket accept failed` once, then continuous `ERROR [NioEndpoint] Error
in selector loop (java/io/IOException: ClosedSelectorException)` until
the process is killed.

No `[SEL]` lines appear in stderr, confirming the natives are
bypassed.

## Related fixes

- `cdcf159` (2026-05-28) — `s2_parse_socket_addr` /
  `p98_extract_socket_addr` handle real-JDK `InetSocketAddress` holder
  layout. This is what unblocked Tomcat from STARTING_PREP; the
  selector gap surfaces *because* the bind is now working.
- `3c288b0` (2026-05-28) — removed synthetic
  `Connector.startInternal` / `AbstractProtocol.start` stubs that
  short-circuited the real Tomcat lifecycle. Without those removed
  the selector error would never have been observable.
- `e2031a2` (2026-05-28) — same shape of real-JDK-layout audit for
  `RandomAccessFile.getFD` / `File.path`. The Lucene `FSDirectory.sync`
  failure documented in `bc-ec-mod-mododdinverse-investigation.md` and
  this Tomcat Selector issue likely share the same dispatch-layer root
  cause; whichever is investigated first will unblock the other.
