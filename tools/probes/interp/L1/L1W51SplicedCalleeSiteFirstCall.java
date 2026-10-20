// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L1: a call site inside a callee the
// compiler SPLICES into its caller resolves when it first runs there
// (JVMS §5.4.3), so the callee class's loader becomes an initiating loader
// of the site's owner then
// (docs/internal/fixed-bugs/interpreter-L1-a-call-site-of-a-spliced-callee-is-never-recorded-as-resolved-FIXED-20261015.md).
// HotSpot's inlined callee keeps its unresolved site as an uncommon trap.
// CratonVM's splice planners bound such a site by name from the callee's
// constant pool and never resolved it, so its first execution inside the
// splice recorded nothing. Since wave 51 a callee with such a site is not
// spliced (`jit_bridge.rs` `resolve_inline_site_from`,
// `callee-site-unresolved`); it is called, and resolves the site where it
// runs.
//
// `inner(int)` is tiny (one trailing return), so `warm`'s compile splices
// it. Rows (the application loader's `getInitiatedClasses` list):
//   snever -- java.util.HexFormat, owner only of an `invokestatic` in a
//             branch of `inner` that never runs;
//   sonce  -- java.util.Base64, owner only of an `invokestatic` in a branch
//             of `inner` taken once, from `warm`'s compiled body, at the end.
// Both owners are loaded up front through `Class.forName(name, false, null)`
// (the bootstrap loader initiates them, not the application loader).
//
// HotSpot 25 prints (agent; `java`, default flags; measured, JDK 25.0.3, two
// runs, and `-Xint` alike):
//     cold snever=false sonce=false
//     hot snever=false sonce=true
//     done
// CratonVM before wave 51, JIT on, once `warm`'s compile splices `inner`:
// `hot snever=false sonce=false`. `--nojit` prints HotSpot's lines.
// Positive control: `CRATONVM_DBG_JITC=1` prints `inline-resolve REFUSED
// L1W51SplicedCalleeSiteFirstCall.inner(I)I ...: callee-site-unresolved`;
// `CRATONVM_JIT_SPLICE_REFUSES_UNRESOLVED_SITE=0` restores the splice.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L1W51SplicedCalleeSiteFirstCall$Agent
// containing L1W51SplicedCalleeSiteFirstCall*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L1W51SplicedCalleeSiteFirstCall
// Without the agent both VMs print "no agent".
import java.lang.instrument.Instrumentation;

public class L1W51SplicedCalleeSiteFirstCall {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static int inner(int k) {
        int r = k & 7;
        if (k == -3) {
            r = java.util.HexFormat.of().toString().length();
        }
        if (k == -4) {
            r = java.util.Base64.getDecoder() == null ? 1 : 2;
        }
        return r;
    }

    static long warm(int from, int rounds) {
        long sum = 0;
        for (int n = 0; n < rounds; n++) {
            sum += inner(from + n);
        }
        return sum;
    }

    static boolean holds(Class<?>[] classes, String name) {
        for (Class<?> k : classes) {
            if (k.getName().equals(name)) {
                return true;
            }
        }
        return false;
    }

    static void row(Instrumentation i, String label) {
        Class<?>[] list = i.getInitiatedClasses(L1W51SplicedCalleeSiteFirstCall.class.getClassLoader());
        System.out.println(label
                + " snever=" + holds(list, "java.util.HexFormat")
                + " sonce=" + holds(list, "java.util.Base64"));
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        Class.forName("java.util.HexFormat", false, null);
        Class.forName("java.util.Base64", false, null);
        row(i, "cold");
        long sum = 0;
        for (int round = 0; round < 6; round++) {
            sum += warm(0, 200000);
            // Give a background compiler time to publish `warm`.
            Thread.sleep(50);
        }
        // `from = -4`, one round: the `once` branch, run by `warm`'s body.
        sum += warm(-4, 1);
        row(i, "hot");
        if (sum < 0) {
            System.out.println(sum);
        }
        System.out.println("done");
    }
}
