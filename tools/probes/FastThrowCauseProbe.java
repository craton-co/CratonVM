// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Do stackless fast-throw exceptions carry `Throwable`'s two field
 * initialisers?
 *
 * Opt-in door: `CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1`. A hot implicit-
 * exception site then gets an exception the VM ALLOCATES without running any
 * constructor (`exceptions::create_stackless_exception_object`), so
 * `cause = this` and `suppressedExceptions = SUPPRESSED_SENTINEL` are the VM's
 * to write. Each caught exception here is a fresh object, and the door's
 * contract is that a first `initCause` succeeds and `addSuppressed` is kept,
 * as on a constructed one.
 *
 * Measured 2026-09-24, 200,000 throws over three sites (NPE, / by zero,
 * AIOOBE), both `--jdk-only` and `--compatible`:
 *
 *   flag off                  stackless=0        causeRefused=0        OK
 *   flag on, before the fix   stackless=132,265  causeRefused=132,265  FAIL
 *   flag on, after the fix    stackless=~132k    causeRefused=0        OK
 *
 * Before the fix the door mirrored `cause` only when the slot read back as
 * `Int(0)`, the never-written LEGACY 16-byte cell. `Throwable`'s instances are
 * COMPACT, where a never-written reference slot reads `Object(None)`, so the
 * mirror never fired. `suppressedLost` was 0 throughout: that mirror tests for
 * "already a list", which reads the same in both layouts.
 *
 * HotSpot 25 prints FAIL here, for a different reason: its fast-throw
 * exception is ONE shared, preallocated instance per kind carrying neither
 * initialiser, so even the first stackless NPE refuses `initCause` and drops
 * `addSuppressed`. CratonVM deliberately hands out a fresh object per throw
 * with constructed-object semantics. Compare CratonVM against its own flag-off
 * run, not against HotSpot.
 *
 *   CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1 cratonvm -cp . FastThrowCauseProbe
 */
public class FastThrowCauseProbe {
    static int npe(Object o) { return o.hashCode(); }
    static int div(int a, int b) { return a / b; }
    static int aioobe(int[] a, int i) { return a[i]; }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        int stackless = 0, causeRefused = 0, suppressedLost = 0, total = 0;
        RuntimeException seed = new RuntimeException("seed");
        int[] arr = new int[1];
        for (int i = 0; i < n; i++) {
            RuntimeException e = null;
            try {
                switch (i % 3) {
                    case 0: npe(null); break;
                    case 1: div(i, 0); break;
                    default: aioobe(arr, 5); break;
                }
            } catch (RuntimeException x) {
                e = x;
            }
            total++;
            if (e.getStackTrace().length == 0) stackless++;
            try {
                e.initCause(seed);
                if (e.getCause() != seed) causeRefused++;
            } catch (IllegalStateException ise) {
                causeRefused++;
            }
            e.addSuppressed(seed);
            if (e.getSuppressed().length != 1) suppressedLost++;
        }
        System.out.println("total=" + total + " stackless=" + stackless
                + " causeRefused=" + causeRefused + " suppressedLost=" + suppressedLost);
        // Control: a genuine null cause is still refused.
        boolean cnfeRefused;
        try { new ClassNotFoundException().initCause(seed); cnfeRefused = false; }
        catch (IllegalStateException ise) { cnfeRefused = true; }
        System.out.println("cnfe refused=" + cnfeRefused);
        System.out.println(causeRefused == 0 && suppressedLost == 0 && cnfeRefused
                ? "FastThrowCauseProbe OK" : "FastThrowCauseProbe FAIL");
    }
}
