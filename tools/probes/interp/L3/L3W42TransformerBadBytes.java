// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L3 -- evidence for
// docs/internal/fixed-bugs/interpreter-L3-a-load-time-transform-that-returns-a-bad-class-file-is-dropped-FIXED-20261007.md:
// a transformer that answers a class-path class's first load with bytes that
// are not a class file, or with another class's class file, makes the load
// fail on HotSpot; CratonVM drops the transformer's answer and loads the
// class as it is on the class path.
//
// Rows:
//   load-garbage        -- first touch of `Garbage`, whose load the
//                          transformer answers with eight bytes;
//   load-garbage-again  -- the same class touched again;
//   load-wrong-name     -- `WrongName`'s load answered with `Donor`'s bytes;
//   retransform-garbage -- `retransformClasses(Retrans)` answered with eight
//                          bytes (the redefinition check's path, not the
//                          load's), then `Retrans.value()`.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     load-garbage: java.lang.ClassFormatError: Incompatible magic value 16909060 in class file L3W42TransformerBadBytes$Garbage
//     load-garbage-again: java.lang.ClassFormatError: Incompatible magic value 16909060 in class file L3W42TransformerBadBytes$Garbage
//     load-wrong-name: java.lang.NoClassDefFoundError: L3W42TransformerBadBytes$WrongName (wrong name: L3W42TransformerBadBytes$Donor)
//     retransform-garbage: java.lang.ClassFormatError: null
// CratonVM (read from the code, not run; both modes;
// `instrument::pre_transform_for_load` refuses bytes whose `this_class` is
// not the class's name and keeps the class-path class file):
//     load-garbage: returned 1
//     load-garbage-again: returned 1
//     load-wrong-name: returned 2
//     retransform-garbage: java.lang.ClassFormatError: null
// Since wave 43 (lane L3) the transformer's answer is staged whatever it is
// and the load that consumes it fails (`instrument::pre_transform_for_load`,
// the definer's refusals in `ClassManager::define_class_shared_with_options`),
// and the offer is given back so the second touch runs the transformer and
// fails again: HotSpot's four lines, both modes (predicted from the code; the
// host run is the check). `CRATONVM_DBG_RETRANSFORM=1` prints
// `[redefine] load-time transform of ... staged a class file that does not
// define it` for the first three rows (the base prints no such line).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W42TransformerBadBytes$Agent
//     Can-Retransform-Classes: true
// containing L3W42TransformerBadBytes*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W42TransformerBadBytes
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W42TransformerBadBytes {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Garbage {
        public static int value() {
            return 1;
        }
    }

    public static class WrongName {
        public static int value() {
            return 2;
        }
    }

    public static class Donor {
        public static int value() {
            return 3;
        }
    }

    public static class Retrans {
        public static int value() {
            return 4;
        }
    }

    static byte[] donorBytes;
    static volatile boolean garbageRetransform;

    static final class Bad implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String name, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if ("L3W42TransformerBadBytes$Garbage".equals(name)) {
                return new byte[] {1, 2, 3, 4, 5, 6, 7, 8};
            }
            if ("L3W42TransformerBadBytes$WrongName".equals(name)) {
                return donorBytes;
            }
            if (garbageRetransform && "L3W42TransformerBadBytes$Retrans".equals(name)) {
                return new byte[] {1, 2, 3, 4, 5, 6, 7, 8};
            }
            return null;
        }
    }

    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = "returned " + r;
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        try (InputStream in = L3W42TransformerBadBytes.class
                .getResourceAsStream("L3W42TransformerBadBytes$Donor.class")) {
            donorBytes = in.readAllBytes();
        }
        Retrans.value();
        i.addTransformer(new Bad(), true);
        row("load-garbage", () -> Garbage.value());
        row("load-garbage-again", () -> Garbage.value());
        row("load-wrong-name", () -> WrongName.value());
        garbageRetransform = true;
        row("retransform-garbage", () -> {
            i.retransformClasses(Retrans.class);
            return Retrans.value();
        });
    }
}
