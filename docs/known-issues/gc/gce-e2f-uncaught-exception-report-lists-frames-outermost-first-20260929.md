# The launcher's uncaught-exception report lists stack frames outermost first

> **STATUS (2026-09-29, gce e2/f): OPEN -- found while reading a probe's
> stderr; not a GC defect, filed so it is not lost.** Owner: whoever owns
> `vm-cli`'s `main-vm run() returned Err` report. Size: XS.

*Filed 2026-09-29 by gce wave e2, lane f.*

## Evidence

```java
public class TraceOrder {
    static void inner() { throw new IllegalStateException("x"); }
    static void outer() { inner(); }
    public static void main(String[] a) { outer(); }
}
```

HotSpot 25.0.3 prints `at TraceOrder.inner(TraceOrder.java:2)`, then
`outer`, then `main`. The base binary (`adb9178bc`, Windows) prints

```
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/IllegalStateException: x
	at TraceOrder.main(TraceOrder.java:4)
	at TraceOrder.outer(TraceOrder.java:3)
	at TraceOrder.inner(TraceOrder.java:2)
```

(and the same again on the `Err (debug)` line). The `NativeGrowthReclaimProbe`
OOME reads `at ...main(...:95)` above `at ...fillAndDrop(...:131)`, which
made the throw site look like `main`.

## Impact

Every uncaught-exception triage on stderr reads the innermost frame LAST,
the opposite of the JVM convention; a reader who takes the first frame as
the throw site is misled. The class name also uses `/` where HotSpot prints
`.` (`java/lang/IllegalStateException`).

## Fix

Print the frames in the throwable's own order (index 0 = innermost), and
the class name in its binary name form, in the formatter behind that line;
or route the report through `Throwable.printStackTrace` as HotSpot's
`dispatchUncaughtException` does.

## How to verify

The program above prints HotSpot's three `at` lines in HotSpot's order.
