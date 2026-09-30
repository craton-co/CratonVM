# Proposal: ephemeron semantics for weak-keyed native side tables

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 27
> of 54).** Not built (the JUL and TLS side tables still report their values
> from root scans). **Gate:** the LoaderUnloadProbe variant (a webapp JUL
> handler left installed at undeploy) unloads its loader on every backend.
> **Size:** M-L (a fixpoint in three markers, a barrier for the concurrent
> ones).

**Status: PROPOSAL** (filed 2026-09-24, gc-common w14-d, on `5bb2773e6`).
**Scope:** common (the VM<->GC root protocol), with a marker hook in each
collector. Not a defect page. It describes the one gap the wave-11 weak-row
pattern cannot close.

## The pattern it would complete

Waves 11 to 14 made several `native-builtins` side tables weak in their key
object:

* the Locale subtag tables (`lib.rs::gc_sweep_locale_rows`, w11-d);
* the TLS owner rows (`t27_tls::gc_sweep_tls_rows`, w12-a / w13-a);
* the JUL per-object rows and the Tomcat JULI registries
  (`logmanager::gc_sweep_logging_rows`, w14-d).

A row's key is not a root and the row is dropped after the collection that
found the key dead. A row whose VALUE is an object keeps that value as a
strong root (`gc_scan_jul_side_rows`, for example), because the
value stands in for a field of the key that our object shapes cannot hold
(the TLS tables report theirs from their `gc_scan_tls_*` root scans).

## The gap

A strong-root value is live whether or not its key is. If the value's own
object graph reaches the key, the key is always marked and the row never
goes. HotSpot collects the same cycle, because there the value hangs off a
field of the key and dies with it.

A concrete case (w14-d): a JULI webapp root logger (key) -> its handler list
(value, rooted) -> a handler whose class the webapp defined -> the webapp
class loader -> a class static that holds a child logger -> its parent, the
root logger. JULI's own undeploy path breaks this cycle, because it calls
`removeHandler` on every logger and w14-d makes that empty the rows. A
framework that drops a logger graph without removing its handlers leaks the
loader in CratonVM, and HotSpot does not.

## Proposed direction

Make the value half of such a row an ephemeron: trace the value only once the
key is marked.

1. A per-VM registry of "ephemeron sources". Each is a callback that, given
   a key-liveness predicate, pushes the values of the rows whose key is
   marked. The logging and TLS modules would register one each, instead of
   reporting their values from the root scan. (The Locale rows hold only
   strings and need none.)
2. In each collector's marker: after the root and transitive closure, call
   every ephemeron source with "is marked" and trace what it pushes. Iterate
   to a fixpoint, because new marks can make more keys live. Only then run
   the existing weak-row sweeps. This is the `java.lang.ref` / JVMTI
   ephemeron loop HotSpot runs, and it belongs next to the reference
   processor's discovery.
3. Concurrent markers (G1 concurrent mark, ZGC, the Generational
   concurrent old-gen mark) need a keep-alive barrier on a read that hands a
   value out of a table during marking, as `Reference.get` has. Without one,
   a value read out and stored into the heap after its source ran can be
   missed. Until that barrier exists, the concurrent markers would have to
   run the ephemeron loop in the remark pause only.

## Why not now

Step 2 lives inside the three collectors' markers, which this round does not
own. Step 3 is a barrier change. A partial version (ephemeron values for the
STW collectors only) would make weak-row reachability differ between
backends, which is worse than the uniform strong-value rule in place today.

## Confirmation

```
rg -n "gc_scan_jul_side_rows|fn gc_scan_tls_ctx_|gc_sweep_logging_rows" native-builtins/src
```

Each weak-keyed table reports its values from a root scan today.

## What would retire it

An ephemeron loop in all three markers, with the logging and TLS
tables moved from root scans onto it. A LoaderUnloadProbe variant in which a
webapp-defined JUL handler stays installed on a root logger at undeploy (no
`removeHandler`) would unload its loader on every backend, as it does on
HotSpot.
