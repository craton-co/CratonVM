import java.lang.reflect.Array;
import java.util.*;

/**
 * Interpreter round i5 wave 104, lane L3 (hunt): a throwable raised INSIDE a
 * JDK `native` method lists that method on top as `(Native Method)`, then the
 * JDK Java frames down to the probe, on HotSpot 25. CratonVM serves most of
 * these natives in Rust: the trace must still name the native and its JDK
 * callers (served-raise marks, `stackwalker::served_raise_frames`), and the
 * message must be HotSpot's.
 *
 * Each row: "<name>: <class> <message> [<frames above the probe, innermost
 * first, as class.method, (N) for a native frame>] <first probe frame>". No
 * line numbers (they differ between JDK builds).
 *
 * Run: java -cp . L3W104NativeThrowFrames, both modes, JIT and --nojit.
 * Expected: HotSpot 25's output (every row is deterministic).
 */
public class L3W104NativeThrowFrames {
    interface Body { void run() throws Throwable; }

    static class NotCloneable {
        Object copy() throws CloneNotSupportedException { return super.clone(); }
    }

    static void row(String name, Body b) {
        Throwable t = null;
        try {
            b.run();
        } catch (Throwable x) {
            t = x;
        }
        if (t == null) {
            System.out.println(name + ": no exception");
            return;
        }
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder jdk = new StringBuilder();
        int i = 0;
        while (i < st.length && !st[i].getClassName().startsWith("L3W104NativeThrowFrames")) {
            jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
            if (st[i].isNativeMethod()) jdk.append("(N)");
            i++;
        }
        String first = i < st.length ? st[i].getClassName() + "." + st[i].getMethodName() : "-";
        // Lambda bodies: fold the synthetic index.
        first = first.replaceAll("lambda\\$main\\$\\d+", "lambda\\$main");
        System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
    }

    public static void main(String[] args) {
        int[] ints = new int[10];
        String[] strs = new String[2];
        row("arraycopy-null-src", () -> System.arraycopy(null, 0, ints, 0, 1));
        row("arraycopy-null-dst", () -> System.arraycopy(ints, 0, null, 0, 1));
        row("arraycopy-type", () -> System.arraycopy(ints, 0, strs, 0, 1));
        row("arraycopy-not-array", () -> System.arraycopy("x", 0, ints, 0, 1));
        row("arraycopy-src-bounds", () -> System.arraycopy(ints, 5, new int[10], 0, 6));
        row("arraycopy-negative", () -> System.arraycopy(ints, -1, new int[10], 0, 1));
        row("arraycopy-length", () -> System.arraycopy(ints, 0, new int[10], 0, -2));
        row("arraycopy-store", () -> System.arraycopy(new Object[] {1}, 0, strs, 0, 1));
        row("newInstance-negative", () -> Array.newInstance(int.class, -1));
        row("newInstance-void", () -> Array.newInstance(void.class, 1));
        row("newInstance-null", () -> Array.newInstance(null, 1));
        row("newInstance-empty-dims", () -> Array.newInstance(int.class, new int[0]));
        row("newInstance-negative-dim", () -> Array.newInstance(int.class, 2, -3));
        row("get-not-array", () -> Array.get("x", 0));
        row("get-null", () -> Array.get(null, 0));
        row("get-bounds", () -> Array.get(ints, 10));
        row("getLength-not-array", () -> Array.getLength(1));
        row("getInt-reference-array", () -> Array.getInt(strs, 0));
        row("getInt-widening-refused", () -> Array.getInt(new long[1], 0));
        row("set-mismatch", () -> Array.set(ints, 0, "s"));
        row("set-null-primitive", () -> Array.set(ints, 0, null));
        row("setInt-bounds", () -> Array.setInt(ints, -1, 3));
        row("clone-not-cloneable", () -> new NotCloneable().copy());
        row("holdsLock-null", () -> Thread.holdsLock(null));
        row("wait-not-owner", () -> new Object().wait());
        row("wait-timed-not-owner", () -> new Object().wait(5));
        row("notify-not-owner", () -> new Object().notify());
        row("notifyAll-not-owner", () -> new Object().notifyAll());
        row("identity-of-null", () -> {
            if (System.identityHashCode(null) != 0) throw new AssertionError("non-zero");
        });
        row("arraycopy-null-src-again", () -> System.arraycopy(null, 0, ints, 0, 1));
    }
}
