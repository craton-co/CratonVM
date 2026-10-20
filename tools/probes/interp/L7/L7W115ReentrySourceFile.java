// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe
//
// Interpreter round i6 wave 115, lane L7: the source file and line of a frame a native's
// re-entry into Java pushes (`interpreter::execute`), under every arm of the re-entry
// switches: `CRATONVM_EXECUTE_LEAN_REENTRY` (the source file is read only by a reader that
// needs it), `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME` (the frame is pushed from a per-VM record,
// which since wave 115 reads the deferred source file only when the record is built), and
// `CRATONVM_EXECUTE_METHOD_INDEX_MEMO` (the method is found by a memoised declared index).
// Each row re-enters a method through reflection or a method handle many times (so the record
// and the memo are warm and, with the JIT, the callee compiles), and prints the file name and
// line of the callee's frame as a throwable's trace and a StackWalker see it.
//
// Run, every arm (unset, each switch =1 alone, all three =1), --jdk-only and --compatible, with
// and without --nojit:
//   cratonvm --java-home <jdk25> --jdk-only -cp <dir> L7W115ReentrySourceFile
//   CRATONVM_EXECUTE_LEAN_REENTRY=1 CRATONVM_EXECUTE_CACHED_REENTRY_FRAME=1 \
//     CRATONVM_EXECUTE_METHOD_INDEX_MEMO=1 cratonvm --java-home <jdk25> --jdk-only -cp <dir> L7W115ReentrySourceFile
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// reflect-throw java.lang.IllegalStateException: last 19999 at thrower@L7W115ReentrySourceFile.java:39 sum=199990000
// mh-throw java.lang.IllegalStateException: last 19999 at thrower@L7W115ReentrySourceFile.java:39 sum=199990000
// reflect-walk walker@L7W115ReentrySourceFile.java:48
// caught caught@L7W115ReentrySourceFile.java:55
// reflect-caught sum=199970000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L7W115ReentrySourceFile {
    static final int ROUNDS = 20_000;

    static int thrower(int x) {
        if (x == ROUNDS - 1) {
            throw new IllegalStateException("last " + x);
        }
        return x + 1;
    }

    static String walker(int x) {
        if (x != ROUNDS - 1) {
            return "";
        }
        StackWalker.StackFrame top = StackWalker.getInstance().walk(s -> s.findFirst().orElseThrow());
        return top.getMethodName() + "@" + top.getFileName() + ":" + top.getLineNumber();
    }

    static int caught(int x) {
        try {
            if (x == ROUNDS - 1) {
                throw new IllegalArgumentException("in " + x);
            }
            return x;
        } catch (IllegalArgumentException e) {
            StackTraceElement f = e.getStackTrace()[0];
            System.out.println("caught " + f.getMethodName() + "@" + f.getFileName() + ":" + f.getLineNumber());
            return -1;
        }
    }

    static String where(Throwable t) {
        StackTraceElement f = t.getStackTrace()[0];
        return t.getClass().getName() + ": " + t.getMessage() + " at " + f.getMethodName() + "@"
                + f.getFileName() + ":" + f.getLineNumber();
    }

    public static void main(String[] args) throws Throwable {
        Method mThrower = L7W115ReentrySourceFile.class.getDeclaredMethod("thrower", int.class);
        long sum = 0;
        String reflected = "none";
        for (int i = 0; i < ROUNDS; i++) {
            try {
                sum += (Integer) mThrower.invoke(null, i);
            } catch (InvocationTargetException e) {
                reflected = where(e.getCause());
            }
        }
        System.out.println("reflect-throw " + reflected + " sum=" + sum);

        MethodHandle hThrower = MethodHandles.lookup().findStatic(
                L7W115ReentrySourceFile.class, "thrower", MethodType.methodType(int.class, int.class));
        sum = 0;
        String handled = "none";
        for (int i = 0; i < ROUNDS; i++) {
            try {
                sum += (int) hThrower.invokeExact(i);
            } catch (IllegalStateException e) {
                handled = where(e);
            }
        }
        System.out.println("mh-throw " + handled + " sum=" + sum);

        Method mWalker = L7W115ReentrySourceFile.class.getDeclaredMethod("walker", int.class);
        String walked = "";
        for (int i = 0; i < ROUNDS; i++) {
            String w = (String) mWalker.invoke(null, i);
            if (!w.isEmpty()) {
                walked = w;
            }
        }
        System.out.println("reflect-walk " + walked);

        Method mCaught = L7W115ReentrySourceFile.class.getDeclaredMethod("caught", int.class);
        sum = 0;
        for (int i = 0; i < ROUNDS; i++) {
            sum += (Integer) mCaught.invoke(null, i);
        }
        System.out.println("reflect-caught sum=" + sum);
    }
}
