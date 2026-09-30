// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L5: `Instrumentation.getInitiatedClasses`
// lists a JDK class a loader's class names by a `CONSTANT_Class` only when
// the constant was RESOLVED
// (docs/known-issues/interpreter/i44-L5-getinitiatedclasses-lists-a-jdk-class-an-unresolved-constant-names-20261008.md).
//
// Rows (each `true`/`false`), the application loader's list:
//   unresolved -- java.util.concurrent.ConcurrentHashMap, named only by an
//                 `ldc` in `never()`, which never runs (the JDK's start-up
//                 loaded the class, so it exists);
//   resolved   -- java.util.concurrent.ConcurrentLinkedDeque, named by an
//                 `ldc` in `once()`, which runs;
//   new        -- java.util.concurrent.CopyOnWriteArraySet, by `new` in
//                 `once()`;
//   array      -- java.util.concurrent.Semaphore, by `anewarray` in `once()`
//                 (an array resolution files its element class).
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     unresolved=false resolved=true new=true array=true
// CratonVM on the base `55834015b` (from the code): the list scans every
// `CONSTANT_Class` of the loader's classes, resolved or not, so
// `unresolved=true resolved=true new=true array=true`. Positive control: the
// `unresolved` row itself (no debug line: the record is read only by this
// list).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L5W46InitiatedUnresolvedConstant$Agent
// containing L5W46InitiatedUnresolvedConstant*.class, then
//     java|cratonvm [--nojit] -javaagent:probe.jar -cp probe.jar L5W46InitiatedUnresolvedConstant
// Without the agent both VMs print "no agent". `--compatible` output differs
// by design: it prints the base's line, `unresolved=true resolved=true
// new=true array=true` (measured on the wave-46 host run; the record is
// armed under `--jdk-only` only and the wave-44 scan is unchanged there).
import java.lang.instrument.Instrumentation;

public class L5W46InitiatedUnresolvedConstant {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static Class<?> never() {
        return java.util.concurrent.ConcurrentHashMap.class;
    }

    static Object[] once() {
        Object[] out = new Object[3];
        out[0] = java.util.concurrent.ConcurrentLinkedDeque.class;
        out[1] = new java.util.concurrent.CopyOnWriteArraySet<Object>();
        out[2] = new java.util.concurrent.Semaphore[1];
        return out;
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
            never();
        }
        once();
        Class<?>[] app = i.getInitiatedClasses(L5W46InitiatedUnresolvedConstant.class.getClassLoader());
        StringBuilder sb = new StringBuilder();
        sb.append("unresolved=").append(holds(app, "java.util.concurrent.ConcurrentHashMap"));
        sb.append(" resolved=").append(holds(app, "java.util.concurrent.ConcurrentLinkedDeque"));
        sb.append(" new=").append(holds(app, "java.util.concurrent.CopyOnWriteArraySet"));
        sb.append(" array=").append(holds(app, "java.util.concurrent.Semaphore"));
        System.out.println(sb);
    }
}
