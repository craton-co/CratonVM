// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L5 -- a KNOWN DIFFERENCE, recorded, not
// asserted by the orchestrator's diff: the verifier's assignability loads.
// HotSpot's split verifier checks `areturn` of a `LinkedTransferQueue` from a
// method declared to return `AbstractQueue` by loading BOTH classes through the
// verified class's loader (`VerificationType::is_reference_assignable_from`),
// so the application loader initiates them although the method never runs.
// CratonVM's verifier records no initiating load
// (`docs/internal/fixed-bugs/interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007.md`,
// item 4).
//
// Rows (each `true`/`false`, the application loader's
// `Instrumentation.getInitiatedClasses` list after `Holder` is linked by a
// call that does not run `never`):
//   target  -- java.util.AbstractQueue, the declared return type
//   source  -- java.util.concurrent.LinkedTransferQueue, the returned type
//   iface   -- java.util.concurrent.TransferQueue, an INTERFACE target: the
//              verifier treats an interface as Object, but loads the target
//              to learn that it is one
//
// HotSpot 25.0.3 prints (agent, default flags and -Xint; measured):
//   target=true source=true iface=true touched=1
// CratonVM (debug build of the wave-55 lane, Windows, `--jdk-only` and
// `--compatible`; the known difference):
//   target=false source=false iface=false touched=1
//
// SETUP: a jar whose manifest has
//     Premain-Class: L5W55VerifierInitiatedLoads$Agent
// containing L5W55VerifierInitiatedLoads*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L5W55VerifierInitiatedLoads
// Without the agent both VMs print "no agent".
import java.lang.instrument.Instrumentation;

public class L5W55VerifierInitiatedLoads {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static class Holder {
        static int touch() {
            return 1;
        }

        static java.util.AbstractQueue<Object> never() {
            return new java.util.concurrent.LinkedTransferQueue<Object>();
        }

        static java.util.concurrent.TransferQueue<Object> neverIface() {
            return new java.util.concurrent.LinkedTransferQueue<Object>();
        }
    }

    static boolean holds(Class<?>[] classes, String name) {
        for (Class<?> k : classes) {
            if (k.getName().equals(name)) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) {
        if (inst == null) {
            System.out.println("no agent");
            return;
        }
        int t = Holder.touch();
        Class<?>[] init = inst.getInitiatedClasses(L5W55VerifierInitiatedLoads.class.getClassLoader());
        System.out.println("target=" + holds(init, "java.util.AbstractQueue")
                + " source=" + holds(init, "java.util.concurrent.LinkedTransferQueue")
                + " iface=" + holds(init, "java.util.concurrent.TransferQueue")
                + " touched=" + t);
    }
}
