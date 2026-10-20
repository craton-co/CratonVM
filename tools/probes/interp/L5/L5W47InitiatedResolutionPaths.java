// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L5: the application loader's
// `Instrumentation.getInitiatedClasses` list holds exactly the JDK classes
// the loader initiated -- by a resolution, whatever door made it -- and not a
// JDK class a constant names that was never resolved
// (docs/internal/fixed-bugs/interpreter-L5-getinitiatedclasses-lists-a-jdk-class-an-unresolved-constant-names-FIXED-20261011.md).
//
// Rows (each `true`/`false`), the application loader's list:
//   string      -- java.lang.String: `main(String[])`'s parameter, which the
//                  launcher's `getMethod("main", ...)` resolves through the
//                  main class's loader before `main` runs;
//   launcher    -- java.util.concurrent.ConcurrentHashMap, only the parameter
//                  type of a PUBLIC method never called (the launcher's
//                  `getMethod` lists the class's public methods);
//   package     -- java.util.concurrent.ConcurrentLinkedDeque, only the
//                  parameter type of a package-private method never called;
//   owner-never -- java.util.concurrent.atomic.LongAdder, only the owner of a
//                  method reference in a method never called;
//   owner-once  -- java.util.Objects, only the owner of an `invokestatic`
//                  that runs;
//   forname     -- java.util.concurrent.ConcurrentLinkedQueue, only named by
//                  `Class.forName(name, false, <the application loader>)`;
//   indy-type   -- java.lang.Thread$State, only a parameter type of a string
//                  concatenation call site that runs. HotSpot resolves the
//                  site's `MethodType` with the NULL loader while its types
//                  are "always visible" (public, `java.lang` or
//                  `java.lang.invoke`), so the application loader does not
//                  initiate it (`find_method_handle_type`; probe
//                  `L5W48MethodTypeConstantLoaderAsks`).
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3; the
// `indy-type` value was recorded as `true` until wave 48, re-measured `false`):
//     string=true launcher=true package=false owner-never=false owner-once=true forname=true indy-type=false
// CratonVM on the base `a451e608e` (from the code, `--jdk-only`): the record
// sees the class door only and the list scans member owners, so
// `string=false launcher=false package=false owner-never=true owner-once=true
// forname=false indy-type=false`; `--compatible` scans every constant.
// Positive control of the wave-47 records: `CRATONVM_DBG_RETRANSFORM=1`
// prints `[RETRANSFORM] getInitiatedClasses(...): recorded <name>` per
// recorded class, and `... resolved class-entry rows (table on)`.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L5W47InitiatedResolutionPaths$Agent
// containing L5W47InitiatedResolutionPaths*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L5W47InitiatedResolutionPaths
// Without the agent both VMs print "no agent".
import java.lang.instrument.Instrumentation;

public class L5W47InitiatedResolutionPaths {
    static volatile Instrumentation inst;
    static volatile String sink;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static void launcher(java.util.concurrent.ConcurrentHashMap<?, ?> map) {
    }

    static void pkg(java.util.concurrent.ConcurrentLinkedDeque<?> deque) {
    }

    static long never(Object adder) {
        return ((java.util.concurrent.atomic.LongAdder) adder).sum();
    }

    static Object once() {
        return java.util.Objects.requireNonNullElse(null, "x");
    }

    static boolean holds(Class<?>[] classes, String name) {
        for (Class<?> k : classes) {
            if (k.getName().equals(name)) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        if (args.length > 99) {
            never(null);
        }
        once();
        ClassLoader app = L5W47InitiatedResolutionPaths.class.getClassLoader();
        Class.forName("java.util.concurrent.ConcurrentLinkedQueue", false, app);
        sink = "state " + Thread.currentThread().getState();
        Class<?>[] list = i.getInitiatedClasses(app);
        StringBuilder sb = new StringBuilder();
        sb.append("string=").append(holds(list, "java.lang.String"));
        sb.append(" launcher=").append(holds(list, "java.util.concurrent.ConcurrentHashMap"));
        sb.append(" package=").append(holds(list, "java.util.concurrent.ConcurrentLinkedDeque"));
        sb.append(" owner-never=").append(holds(list, "java.util.concurrent.atomic.LongAdder"));
        sb.append(" owner-once=").append(holds(list, "java.util.Objects"));
        sb.append(" forname=").append(holds(list, "java.util.concurrent.ConcurrentLinkedQueue"));
        sb.append(" indy-type=").append(holds(list, "java.lang.Thread$State"));
        System.out.println(sb);
    }
}
