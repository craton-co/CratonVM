import java.lang.foreign.*;
import java.lang.invoke.*;

/** SBR5 lane ffmlink: downcall cost through whatever `Linker.nativeLinker()` is. ns per call, median of 5 rounds. */
public class Sbr5FfmlinkBench {
    public static void main(String[] a) throws Throwable {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200_000;
        Linker l = Linker.nativeLinker();
        MethodHandle abs = l.downcallHandle(l.defaultLookup().find("abs").get(), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        MethodHandle strlen = l.downcallHandle(l.defaultLookup().find("strlen").get(), FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        try (Arena z = Arena.ofConfined()) {
            MemorySegment s = z.allocateFrom("hello world");
            for (int r = 0; r < 3; r++) { // warm
                for (int i = 0; i < n / 4; i++) { int v = (int) abs.invokeExact(-i); long q = (long) strlen.invokeExact(s); }
            }
            long[] ta = new long[5], ts = new long[5];
            for (int r = 0; r < 5; r++) {
                long t0 = System.nanoTime(); long acc = 0;
                for (int i = 0; i < n; i++) acc += (int) abs.invokeExact(-i);
                ta[r] = (System.nanoTime() - t0) / n;
                t0 = System.nanoTime();
                for (int i = 0; i < n; i++) acc += (long) strlen.invokeExact(s);
                ts[r] = (System.nanoTime() - t0) / n;
                if (acc == 42) System.out.println();
            }
            java.util.Arrays.sort(ta); java.util.Arrays.sort(ts);
            System.out.println("abs ns/call median=" + ta[2] + " all=" + java.util.Arrays.toString(ta));
            System.out.println("strlen ns/call median=" + ts[2] + " all=" + java.util.Arrays.toString(ts));
        }
    }
}
