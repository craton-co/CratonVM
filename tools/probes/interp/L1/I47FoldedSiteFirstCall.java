// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Item 2 of docs/internal/fixed-bugs/interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007.md
// (2026-10-07): the two shapes wave 50's first-execution guard
// (`cratonvm_jit::JitFirstCallCell`) did not cover -- a call site of a
// compiled method that the compile door found unresolved, which the
// compiler then REMOVED, so the site's first execution resolved nothing and
// its owner was never recorded as initiated by the class's loader:
//   unbox    `Long.valueOf(k).longValue()` -- the optimizing tier's box/unbox
//            fold (`ea_ir_bridge::ir_fold_unbox_of_fresh_box`);
//   hash     `Integer.valueOf(k).hashCode()` -- its box-hash fold
//            (`ir_fold_hash_code_of_integer_box`);
//   adapter  `((Double) f.apply(k)).doubleValue()` -- javac's
//            `invokestatic Integer.valueOf; invokeinterface Function.apply;
//            checkcast Double; invokevirtual Double.doubleValue`, the run both
//            tiers fuse into one helper (`tdigest_adapter_members_match`,
//            `IrBuilder::tdigest_adapter_run_matches`); its row is the
//            `Function` owner.
// Each sits in a branch of its own hot method that runs only once, after the
// method was compiled. HotSpot's C1/C2 leave such a site to an uncommon trap,
// which resolves it there.
//
// Rows: the application loader's `getInitiatedClasses` list, before (`cold`)
// and after (`hot`) the three branches ran.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// cold unbox=false hash=false adapter=false
// hot unbox=true hash=true adapter=true
// HOTSPOT_EXPECTED_END
// Positive control: `CRATONVM_DBG_FIRST_CALL=1` (`[first-call] ...`).
//
// SETUP: a jar whose manifest has
//     Premain-Class: I47FoldedSiteFirstCall$Agent
// containing I47FoldedSiteFirstCall*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar I47FoldedSiteFirstCall
import java.lang.instrument.Instrumentation;

public class I47FoldedSiteFirstCall {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static long unboxHot(int k) {
        if (k == -2) {
            return Long.valueOf(k).longValue();
        }
        return k & 7;
    }

    static long hashHot(int k) {
        if (k == -2) {
            return Integer.valueOf(k).hashCode();
        }
        return k & 7;
    }

    static double adapterHot(java.util.function.Function<Object, Object> f, int k) {
        if (k == -2) {
            return ((Double) f.apply(k)).doubleValue();
        }
        return k & 7;
    }

    static boolean holds(Class<?>[] classes, String name) {
        for (Class<?> c : classes) {
            if (c.getName().equals(name)) {
                return true;
            }
        }
        return false;
    }

    static void row(Instrumentation i, String label) {
        Class<?>[] list = i.getInitiatedClasses(I47FoldedSiteFirstCall.class.getClassLoader());
        System.out.println(label
                + " unbox=" + holds(list, "java.lang.Long")
                + " hash=" + holds(list, "java.lang.Integer")
                + " adapter=" + holds(list, "java.util.function.Function"));
    }

    static double warm(int rounds) {
        double sum = 0;
        for (int n = 0; n < rounds; n++) {
            sum += unboxHot(n) + hashHot(n) + adapterHot(null, n);
        }
        return sum;
    }

    /**
     * `Function.identity()`, reached reflectively so that no constant of this
     * class names `Function` (its `apply` answers the `Integer`, so the cast
     * after the call throws: the call has run by then).
     */
    @SuppressWarnings("unchecked")
    static java.util.function.Function<Object, Object> function() throws Exception {
        return (java.util.function.Function<Object, Object>) Class
                .forName("java.util.function.Function", false, null)
                .getMethod("identity")
                .invoke(null);
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        row(i, "cold");
        double sum = warm(300000);
        // Give a background compiler time to publish the three methods first.
        Thread.sleep(300);
        sum += warm(300000);
        sum += unboxHot(-2) + hashHot(-2);
        try {
            sum += adapterHot(function(), -2);
        } catch (ClassCastException expected) {
            // `identity` answered the Integer; `Function.apply` ran.
        }
        row(i, "hot");
        if (sum < 0) {
            System.out.println(sum);
        }
    }
}
