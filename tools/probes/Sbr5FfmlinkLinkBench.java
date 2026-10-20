import java.lang.foreign.*;
import java.lang.invoke.*;
import java.util.*;
public class Sbr5FfmlinkLinkBench {
  static MethodHandle abs, strlen, qsort; static MemorySegment s, arr, stub;
  static int cmp(MemorySegment a, MemorySegment b) { return Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0)); }
  static long absLoop(int n) throws Throwable { long acc = 0; for (int i = 0; i < n; i++) acc += (int) abs.invokeExact(-i); return acc; }
  static long strlenLoop(int n) throws Throwable { long acc = 0; for (int i = 0; i < n; i++) acc += (long) strlen.invokeExact(s); return acc; }
  static long qsortLoop(int n) throws Throwable { for (int i = 0; i < n; i++) { for (int k = 0; k < 32; k++) arr.setAtIndex(ValueLayout.JAVA_INT, k, (k * 7919 + i) % 101); qsort.invokeExact(arr, 32L, 4L, stub); } return arr.getAtIndex(ValueLayout.JAVA_INT, 0); }
  interface L { long run(int n) throws Throwable; }
  static void bench(String name, int n, L l) throws Throwable {
    long acc = l.run(n); long[] t = new long[5];
    for (int r = 0; r < 5; r++) { long t0 = System.nanoTime(); acc += l.run(n); t[r] = (System.nanoTime() - t0) / n; }
    Arrays.sort(t); System.out.println(name + " ns/call median=" + t[2] + " " + Arrays.toString(t) + (acc == 1 ? "!" : ""));
  }
  public static void main(String[] a) throws Throwable {
    int n = Integer.parseInt(a[0]);
    Linker l = Linker.nativeLinker(); SymbolLookup d = l.defaultLookup();
    abs = l.downcallHandle(d.find("abs").get(), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
    strlen = l.downcallHandle(d.find("strlen").get(), FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
    qsort = l.downcallHandle(d.find("qsort").get(), FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
    Arena z = Arena.ofConfined();
    s = z.allocateFrom("hello world"); arr = z.allocate(4 * 32);
    MethodHandle c = MethodHandles.lookup().findStatic(Sbr5FfmlinkLinkBench.class, "cmp", MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
    stub = l.upcallStub(c, FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT), ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT)), z);
    bench("abs", n, Sbr5FfmlinkLinkBench::absLoop); bench("strlen", n, Sbr5FfmlinkLinkBench::strlenLoop); bench("qsort32+upcalls", n / 50, Sbr5FfmlinkLinkBench::qsortLoop);
  }
}
