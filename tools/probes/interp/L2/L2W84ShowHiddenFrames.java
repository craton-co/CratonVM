// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L2 -- `-XX:+ShowHiddenFrames`
// (`VmConfig::show_hidden_frames`). Until this wave the launcher dropped the
// flag in its silent `-XX:` catch-all, so a hidden class's frames stayed out of
// every trace and an NPE raised in one kept its null message whatever the flag
// said. HotSpot (`java_lang_Throwable::fill_in_stack_trace`, `skip_hidden =
// !ShowHiddenFrames`; `JVM_GetExtendedNPEMessage` answers null only for a
// backtrace whose top frame was skipped; `JavaFrameStream` skips hidden frames
// only `!ShowHiddenFrames`) keeps them all.
//
//   npe-message     getMessage() of an NPE raised inside a hidden class's
//                   static method (`Lookup.defineHiddenClass` of L2W84Hid)
//   npe-top-hidden  is the throwable's top frame the hidden class's?
//   trace-self      does Thread.getStackTrace() taken in the hidden method
//                   list the hidden method's frame?
//   walker-self     does a StackWalker (no options) walk started in the
//                   hidden method list it?
//   plain-npe       an ordinary class's NPE: the flag must not move it
//
// Hidden class names carry a `/0x<address>` tail, so only prefixes are
// compared. Compile without -g (the `<parameter1>` spelling depends on it).
//
// Run (stdout must be the block for the run, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W84ShowHiddenFrames.java
//   $B --java-home $J/.. -cp out L2W84ShowHiddenFrames                     -> DEFAULT block
//   $B --compatible --java-home $J/.. -cp out L2W84ShowHiddenFrames        -> DEFAULT block
//   $B -XX:+UnlockDiagnosticVMOptions -XX:+ShowHiddenFrames --java-home $J/.. -cp out L2W84ShowHiddenFrames
//                                                                          -> SHOW block
//   (the same with --compatible; and each also with --nojit)
//   $J/java -cp out L2W84ShowHiddenFrames                                  -> DEFAULT block
//   $J/java -XX:+UnlockDiagnosticVMOptions -XX:+ShowHiddenFrames -cp out L2W84ShowHiddenFrames
//                                                                          -> SHOW block
//
// Derived from the JDK 25 sources named above (not yet re-measured on a
// HotSpot build at the time of writing; refresh from $J/java before reading a
// DIFF).
//
// HOTSPOT_EXPECTED_BEGIN DEFAULT (JDK 25, default flags)
// npe-message null
// npe-top-hidden false
// trace-self false
// walker-self false
// plain-npe Cannot read field "v" because "<parameter1>" is null
// HOTSPOT_EXPECTED_END
//
// HOTSPOT_EXPECTED_BEGIN SHOW (-XX:+UnlockDiagnosticVMOptions -XX:+ShowHiddenFrames)
// npe-message Cannot invoke "Object.hashCode()" because "<parameter1>" is null
// npe-top-hidden true
// trace-self true
// walker-self true
// plain-npe Cannot read field "v" because "<parameter1>" is null
// HOTSPOT_EXPECTED_END
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Function;
import java.util.stream.Stream;

public class L2W84ShowHiddenFrames {
    static final String HIDDEN_PREFIX = "L2W84Hid";

    static final class Node {
        int v;
    }

    static int plain(Node n) {
        return n.v;
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes;
        try (InputStream in = L2W84ShowHiddenFrames.class.getResourceAsStream("/L2W84Hid.class")) {
            bytes = in.readAllBytes();
        }
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        Class<?> hidden = lookup.defineHiddenClass(bytes, true).lookupClass();

        MethodHandle hash = lookup.findStatic(hidden, "hash", MethodType.methodType(int.class, Object.class));
        try {
            int h = (int) hash.invokeExact((Object) null);
            System.out.println("npe-message no-throw " + h);
        } catch (NullPointerException e) {
            System.out.println("npe-message " + e.getMessage());
            StackTraceElement[] st = e.getStackTrace();
            System.out.println("npe-top-hidden " + (st.length > 0 && st[0].getClassName().startsWith(HIDDEN_PREFIX)));
        }

        MethodHandle trace = lookup.findStatic(hidden, "threadTrace", MethodType.methodType(StackTraceElement[].class));
        StackTraceElement[] tt = (StackTraceElement[]) trace.invokeExact();
        boolean self = false;
        for (StackTraceElement e : tt) {
            self |= e.getClassName().startsWith(HIDDEN_PREFIX);
        }
        System.out.println("trace-self " + self);

        MethodHandle walk = lookup.findStatic(hidden, "walk", MethodType.methodType(Object.class, Function.class));
        Function<Stream<StackWalker.StackFrame>, Object> f =
                s -> s.anyMatch(fr -> fr.getClassName().startsWith(HIDDEN_PREFIX));
        Object walked = (Object) walk.invokeExact(f);
        System.out.println("walker-self " + walked);

        try {
            plain(null);
            System.out.println("plain-npe no-throw");
        } catch (NullPointerException e) {
            System.out.println("plain-npe " + e.getMessage());
        }
    }
}

// Defined again as a HIDDEN class from its own bytes (no NestHost attribute:
// a top-level class in the probe's package).
class L2W84Hid {
    public static int hash(Object o) {
        return o.hashCode();
    }

    public static StackTraceElement[] threadTrace() {
        return Thread.currentThread().getStackTrace();
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    public static Object walk(Function f) {
        return StackWalker.getInstance().walk(f);
    }
}
