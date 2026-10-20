// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 87, lane L2 -- what a StackWalker frame answers,
// interpreted and (after 3000 calls) compiled, in both modes:
//
//   frames-*     walk(s -> s.skip(1).limit(2)): the two callers below the
//                walking method, with getDescriptor(), getByteCodeIndex()
//                (javac fixes them: `level2` calls at bci 0, `level1` at bci
//                2), isNativeMethod() and toStackTraceElement() (line cut)
//   stable       every one of the 3000 walks answered the same
//   limit-0      walk(s -> s.limit(0).count())
//   caller-*     getCallerClass() asked by a method of another class that
//                main reached directly, through Method.invoke and through a
//                MethodHandle: main's class each time (the reflection and
//                method-handle frames are skipped)
//   no-retain    getCallerClass() without RETAIN_CLASS_REFERENCE
//   class-ref    getDeclaringClass() without RETAIN_CLASS_REFERENCE
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W87StackWalkerFrames.java
//   $B --java-home $J/.. -cp out L2W87StackWalkerFrames
//   $B --nojit --java-home $J/.. -cp out L2W87StackWalkerFrames
//   $B --compatible --java-home $J/.. -cp out L2W87StackWalkerFrames   (and --nojit)
//   $J/java -cp out L2W87StackWalkerFrames
//
// HOTSPOT_EXPECTED_BEGIN
// frames-0 level2 (I)Ljava/util/List; bci=0 native=false L2W87StackWalkerFrames.level2(L2W87StackWalkerFrames.java)
// frames-1 level1 (JLjava/lang/String;)Ljava/util/List; bci=2 native=false L2W87StackWalkerFrames.level1(L2W87StackWalkerFrames.java)
// stable true
// limit-0 0
// caller-direct L2W87StackWalkerFrames
// caller-reflect L2W87StackWalkerFrames
// caller-handle L2W87StackWalkerFrames
// no-retain java.lang.UnsupportedOperationException: This stack walker does not have RETAIN_CLASS_REFERENCE access
// class-ref java.lang.UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.List;

public class L2W87StackWalkerFrames {
    static final StackWalker RETAIN = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
    static final StackWalker PLAIN = StackWalker.getInstance();

    static List<String> capture() {
        return RETAIN.walk(s -> s.skip(1).limit(2)
                .map(f -> f.getMethodName() + " " + f.getDescriptor() + " bci=" + f.getByteCodeIndex()
                        + " native=" + f.isNativeMethod() + " "
                        + f.toStackTraceElement().toString().replaceAll(":\\d+\\)", ")"))
                .toList());
    }

    static List<String> level2(int a) {
        return capture();
    }

    static List<String> level1(long b, String c) {
        return level2(7);
    }

    public static void main(String[] args) throws Throwable {
        List<String> first = level1(1L, "x");
        boolean stable = true;
        for (int i = 0; i < 3000; i++) {
            if (!level1(i, "y").equals(first)) {
                stable = false;
            }
        }
        for (int i = 0; i < first.size(); i++) {
            System.out.println("frames-" + i + " " + first.get(i));
        }
        System.out.println("stable " + stable);
        System.out.println("limit-0 " + PLAIN.walk(s -> s.limit(0).count()));

        System.out.println("caller-direct " + L2W87Callee.whoCalls().getName());
        Method m = L2W87Callee.class.getDeclaredMethod("whoCalls");
        System.out.println("caller-reflect " + ((Class<?>) m.invoke(null)).getName());
        MethodHandle h = MethodHandles.lookup().findStatic(L2W87Callee.class, "whoCalls",
                MethodType.methodType(Class.class));
        System.out.println("caller-handle " + ((Class<?>) h.invokeExact()).getName());

        try {
            PLAIN.getCallerClass();
            System.out.println("no-retain no-throw");
        } catch (UnsupportedOperationException e) {
            System.out.println("no-retain " + e);
        }
        try {
            PLAIN.walk(s -> s.findFirst().get().getDeclaringClass());
            System.out.println("class-ref no-throw");
        } catch (UnsupportedOperationException e) {
            System.out.println("class-ref " + e);
        }
    }
}

class L2W87Callee {
    static Class<?> whoCalls() {
        return L2W87StackWalkerFrames.RETAIN.getCallerClass();
    }
}
