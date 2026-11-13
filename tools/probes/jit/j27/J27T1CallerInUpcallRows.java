// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 7 (lane T1ACCEPT; page
// docs/feature-designs/native-dispatch-architecture.md):
// the caller class @CallerSensitive / caller-asking JDK methods see when they are called from the
// Java target of an FFM up-call (C -> stub -> target), cold and after 20,000 up-calls (warm: the
// target and the JDK callees compiled). The restricted `MemorySegment.reinterpret` asks
// `Reflection.getCallerClass()` too; its answer is not printable, but it must be the same frame the
// two printable askers name (the target's class), and the call must succeed.
//   java --enable-native-access=ALL-UNNAMED J27T1CallerInUpcallRows [-Dj27.time=true]
//
// HotSpot 25 expected output (exactly these lines without -Dj27.time=true):
//   fact cold-lookup-class J27T1CallerInUpcallRows
//   fact cold-walker-caller J27T1CallerInUpcallRows
//   fact cold-return 7
//   fact warm-lookup-class J27T1CallerInUpcallRows
//   fact warm-walker-caller J27T1CallerInUpcallRows
//   fact warm-return-sum 140000
// Why: `MethodHandles.lookup()` is @CallerSensitive and its lookup class is its caller, `target`;
// `StackWalker.getCallerClass()` called in `who()` answers who()'s caller, `target` again (the
// up-call stub's hidden LambdaForm frames sit below `target`, so neither asker can reach them);
// `reinterpret(4).get(JAVA_INT, 0)` reads ints[0] = 7, and 20,000 warm calls sum to 140,000.
//
// CratonVM check: both modes, with and without CRATONVM_DBG_CALLER=1 (the facts must not change);
// under the DBG switch, `grep -c 'getCallerClass (security walk):'` counts the asks the fast path
// declined (a fast-answered ask prints `(fast)` and then `(security walk, control)`).
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import static java.lang.foreign.ValueLayout.*;

public class J27T1CallerInUpcallRows {
    static final Linker L = Linker.nativeLinker();
    static final StackWalker WALKER = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static String lookupClass;
    static String walkerCaller;

    static Class<?> who() {
        return WALKER.getCallerClass();
    }

    static int target(MemorySegment a) {
        lookupClass = MethodHandles.lookup().lookupClass().getName();
        walkerCaller = who().getName();
        return a.reinterpret(4).get(JAVA_INT, 0);
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    public static void main(String[] args) throws Throwable {
        boolean time = Boolean.getBoolean("j27.time");
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment ints = arena.allocate(JAVA_INT, 2);
            ints.setAtIndex(JAVA_INT, 0, 7);
            FunctionDescriptor fd = FunctionDescriptor.of(JAVA_INT, ADDRESS);
            MethodHandle t = MethodHandles.lookup().findStatic(J27T1CallerInUpcallRows.class, "target",
                MethodType.methodType(int.class, MemorySegment.class));
            MemorySegment stub = L.upcallStub(t, fd, arena);
            MethodHandle call = L.downcallHandle(stub, fd);

            int r = (int) call.invokeExact(ints);
            fact("cold-lookup-class", lookupClass);
            fact("cold-walker-caller", walkerCaller);
            fact("cold-return", r);

            lookupClass = null;
            walkerCaller = null;
            long t0 = System.nanoTime();
            int sum = 0;
            for (int i = 0; i < 20_000; i++) {
                sum += (int) call.invokeExact(ints);
            }
            long t1 = System.nanoTime();
            fact("warm-lookup-class", lookupClass);
            fact("warm-walker-caller", walkerCaller);
            fact("warm-return-sum", sum);
            if (time) {
                System.out.println("time ns-per-upcall " + (t1 - t0) / 20_000);
            }
        }
    }
}
