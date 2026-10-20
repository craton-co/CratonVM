// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L5: a call site that never runs, inside a
// method hot enough to be compiled, does not make the class's loader an
// initiating loader of the site's owner. HotSpot's C1/C2 leave an unresolved
// site to an uncommon trap; CratonVM's compile door asked the method
// resolution core (`MemberResolver::method_ref`) for the `--jdk-only` access
// check, and the core wrote the owner into the resolved class-entry table that
// `Instrumentation.getInitiatedClasses` reads
// (`interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`, item 2).
//
// Rows (the application loader's list, `true`/`false`), each checked first
// before anything is compiled and again after `hot` has run 300000 times:
//   never -- java.util.concurrent.atomic.DoubleAdder, only the owner of an
//            `invokevirtual` in `hot`'s branch that never runs;
//   runs  -- java.util.concurrent.atomic.DoubleAccumulator, only the owner of
//            an `invokevirtual` in `hot`'s branch taken once, at the end
//            (the positive control: an executed site DOES initiate).
// Both classes are loaded up front through `Class.forName(name, false, null)`
// (the bootstrap loader initiates them, not the application loader), so the
// compile door sees loaded owners. `hot` is package-private so the launcher's
// `getMethod` does not list its parameter types; the receivers come in as
// `Object` and are cast in the branch, so the `checkcast`s are in the same
// never / once branches.
//
// Status (wave 49): EVIDENCE for the open item 2 of
// `docs/internal/fixed-bugs/interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007.md`.
// The two rows pull in opposite directions on CratonVM with the JIT: the
// compile door's access peek (`jit_bridge.rs` `CpResolvers::invoke` ->
// `MemberResolver::method_ref` -> `resolve_method_metadata`) writes the owner
// row for EVERY site of the compiled method, so `hot never=true` (HotSpot
// false); a peek that wrote nothing (tried in wave 49, 74f8b01ff8, reverted)
// gave `hot runs=false` (HotSpot true), because the compiled `-2` branch
// runs the call it bound at compile time without ever resolving it. The page
// says what a fix needs.
//
// HotSpot 25 prints (agent; `java`, default flags, and `-Xint`; measured,
// JDK 25.0.3):
//     cold never=false runs=false
//     hot never=false runs=true
// CratonVM, JIT on (default and `--compatible`; from the code, the
// wave-48 behaviour this branch keeps): `hot never=true runs=true` (the
// peek wrote both rows). With 74f8b01ff8 the host printed `hot never=false
// runs=false`. `--nojit` prints HotSpot's lines.
// Positive control: `CRATONVM_DBG_RETRANSFORM=1` prints `[RETRANSFORM]
// getInitiatedClasses(...)` rows; no `DoubleAdder` row may appear.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L5W49CompiledSiteInitiates$Agent
// containing L5W49CompiledSiteInitiates*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L5W49CompiledSiteInitiates
// Without the agent both VMs print "no agent".
import java.lang.instrument.Instrumentation;

public class L5W49CompiledSiteInitiates {
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
        Class<?>[] list = i.getInitiatedClasses(L5W49CompiledSiteInitiates.class.getClassLoader());
        System.out.println(label
                + " never=" + holds(list, "java.util.concurrent.atomic.DoubleAdder")
                + " runs=" + holds(list, "java.util.concurrent.atomic.DoubleAccumulator"));
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        // Both owners loaded -- by the bootstrap loader, which the
        // application loader does not become an initiating loader of -- so
        // the compile door finds them and can bind (and, before wave 49,
        // record) the sites.
        Class.forName("java.util.concurrent.atomic.DoubleAdder", false, null);
        Class.forName("java.util.concurrent.atomic.DoubleAccumulator", false, null);
        row(i, "cold");
        double sum = 0;
        for (int n = 0; n < 300000; n++) {
            sum += hot(n, null, null);
        }
        Object acc = java.util.concurrent.ThreadLocalRandom.current().nextInt(1) == 0
                ? makeAccumulator() : null;
        sum += hot(-2, null, acc);
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
