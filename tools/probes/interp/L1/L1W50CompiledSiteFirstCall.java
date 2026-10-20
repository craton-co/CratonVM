// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L1: a call site of a compiled method
// resolves when it first RUNS (JVMS §5.4.3), so the class's loader becomes an
// initiating loader of the site's owner then, and never for a site that never
// runs. HotSpot's C1/C2 leave an unresolved site to an uncommon trap.
// CratonVM's compile door used to resolve every site of a method it compiled
// (`jit_bridge.rs` `CpResolvers::invoke` -> `MemberResolver::method_ref`),
// which recorded the owner of a site that never ran; a peek that recorded
// nothing (wave 49, reverted) left a site first run in compiled code with no
// record at all. Wave 50 peeks at such a site and guards it in the compiled
// code with a one-shot first-execution resolution
// (`cratonvm_jit::JitFirstCallCell`,
// `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`, item 2).
//
// Rows (the application loader's `getInitiatedClasses` list, `true`/`false`),
// each taken before `hot` is compiled (`cold`) and after it ran ~600000 times
// and its two "once" branches ran (`hot`):
//   vnever  -- java.util.concurrent.atomic.DoubleAdder, owner only of an
//              `invokevirtual` in a branch of `hot` that never runs;
//   vonce   -- java.util.concurrent.atomic.DoubleAccumulator, owner only of an
//              `invokevirtual` in a branch of `hot` taken once, at the end;
//   snever  -- java.util.HexFormat, owner only of an `invokestatic` that
//              never runs;
//   sonce   -- java.util.Base64, owner only of an `invokestatic` taken once.
// Every owner is loaded up front through `Class.forName(name, false, null)`
// (the bootstrap loader initiates it, not the application loader), so the
// compile door sees loaded owners. The receivers come in as `Object` and are
// cast in the branch, so the `checkcast`s are in the same never / once
// branches. `hot` is package-private so the launcher's `getMethod` does not
// list its parameter types.
//
// HotSpot 25 prints (agent; `java`, default flags; measured, JDK 25.0.3):
//     cold vnever=false vonce=false snever=false sonce=false
//     hot vnever=false vonce=true snever=false sonce=true
// CratonVM before wave 50, JIT on: `hot vnever=true ... snever=true` (the
// compile door recorded every site of `hot`). `--nojit` prints HotSpot's lines.
// Positive control: `CRATONVM_DBG_FIRST_CALL=1` prints `[first-call] ...
// unresolved site(s)` for `hot`, a `guard` line per guarded site, and
// `[first-call] resolved at first execution` for the two `once` sites.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L1W50CompiledSiteFirstCall$Agent
// containing L1W50CompiledSiteFirstCall*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L1W50CompiledSiteFirstCall
// Without the agent both VMs print "no agent".
import java.lang.instrument.Instrumentation;

public class L1W50CompiledSiteFirstCall {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static double hot(int k, Object never, Object once) {
        if (k == -1) {
            return ((java.util.concurrent.atomic.DoubleAdder) never).sum();
        }
        if (k == -2) {
            return ((java.util.concurrent.atomic.DoubleAccumulator) once).get();
        }
        if (k == -3) {
            return java.util.HexFormat.of().toString().length();
        }
        if (k == -4) {
            return java.util.Base64.getDecoder() == null ? 1 : 2;
        }
        return k & 7;
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
        Class<?>[] list = i.getInitiatedClasses(L1W50CompiledSiteFirstCall.class.getClassLoader());
        System.out.println(label
                + " vnever=" + holds(list, "java.util.concurrent.atomic.DoubleAdder")
                + " vonce=" + holds(list, "java.util.concurrent.atomic.DoubleAccumulator")
                + " snever=" + holds(list, "java.util.HexFormat")
                + " sonce=" + holds(list, "java.util.Base64"));
    }

    static double warm(int rounds) {
        double sum = 0;
        for (int n = 0; n < rounds; n++) {
            sum += hot(n, null, null);
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        Class.forName("java.util.concurrent.atomic.DoubleAdder", false, null);
        Class.forName("java.util.concurrent.atomic.DoubleAccumulator", false, null);
        Class.forName("java.util.HexFormat", false, null);
        Class.forName("java.util.Base64", false, null);
        row(i, "cold");
        double sum = warm(300000);
        // Give a background compiler time to publish `hot` before the two
        // `once` branches run.
        Thread.sleep(300);
        sum += warm(300000);
        Object acc = java.util.concurrent.ThreadLocalRandom.current().nextInt(1) == 0
                ? makeAccumulator() : null;
        sum += hot(-2, null, acc);
        sum += hot(-4, null, null);
        row(i, "hot");
        if (sum < 0) {
            System.out.println(sum);
        }
    }

    // Built reflectively so that `main` names the class in no constant.
    static Object makeAccumulator() throws Exception {
        return Class.forName("java.util.concurrent.atomic.DoubleAccumulator", true, null)
                .getConstructor(java.util.function.DoubleBinaryOperator.class, double.class)
                .newInstance((java.util.function.DoubleBinaryOperator) Double::sum, 0.0);
    }
}
