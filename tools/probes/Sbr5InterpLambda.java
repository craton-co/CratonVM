import java.util.*;
import java.util.function.*;

/**
 * SBR5 interp lane: behaviour of interpreted lambda calls (the stackless lambda door). Every shape
 * is exercised well past the door's first-call fill, in loops, and the output is diffed against
 * HotSpot. Positive control: with the argument "deep" it recurses 4000 levels through a lambda; the
 * nested-interpreter path capped that at 2000 (StackOverflowError under --nojit), HotSpot and the door
 * do not. The default run recurses 500 levels: with the JIT on, a recursion whose impl gets compiled
 * mid-descent runs the rest on the native stack, and 4000 levels of that overflow it on a loaded host
 * (base and door alike), so the JIT-on gates use the shallow depth.
 * usage: Sbr5InterpLambda [deep]
 */
public class Sbr5InterpLambda {
    interface CharF { char f(char c); }
    interface ByteF { byte f(byte b); }
    interface ShortF { short f(short s); }
    interface BoolF { boolean f(boolean b); }
    interface LongF { long f(long a, long b, int c); }
    interface MixF { double f(int a, long b, float c, double d, Object o); }
    interface VoidF { void f(int a, String s); }
    interface RefF { Object f(Object o); }
    interface Rec { int f(int n); }

    static long sink;
    static int counter;
    static int thrower(int i) { if (i % 7 == 3) throw new IllegalStateException("boom" + i); return i; }
    static int staticTwo(int a, int b) { return a * 31 + b; }
    int inst = 5;
    int instMethod(int x) { return x + inst; }

    static void run(String name, Runnable r) {
        try { r.run(); } catch (Throwable t) { System.out.println(name + " threw " + t.getClass().getName() + ": " + t.getMessage()); }
    }

    static Rec deepLambda;

    public static void main(String[] args) {
        final int N = 3000;
        // 1. non-capturing and capturing static impls, int/long/double/float/boolean/char/byte/short/ref
        int ci = 11; long cl = 1L << 40; double cd = 2.5; float cf = 1.5f; boolean cb = true; char cc = 'x'; byte cby = 7; short csh = 300; Object co = "obj"; String cnull = null;
        IntSupplier s0 = () -> 7;
        IntBinaryOperator s1 = (a, b) -> a - b;
        LongF s2 = (a, b, c) -> a * 3 + b + c + cl;
        MixF s3 = (a, b, c, d, o) -> a + b + c + d + ci + cl + cf + cd + (cb ? 1 : 0) + cc + cby + csh + (o == null ? -1 : o.hashCode() % 10) + (co == null ? 0 : 1) + (cnull == null ? 100 : 0);
        CharF s4 = c -> (char) (c + cc);
        ByteF s5 = b -> (byte) (b + cby);
        ShortF s6 = x -> (short) (x + csh);
        BoolF s7 = b -> !b == cb;
        VoidF s8 = (a, s) -> counter += a + (s == null ? 1 : s.length());
        RefF s9 = o -> o == null ? cnull : o.toString() + ci;
        Supplier<String> s10 = () -> "const";
        Function<Integer, Integer> s11 = x -> x + 1;
        BiFunction<String, Integer, String> s12 = (s, n) -> s + n + ci;
        DoubleUnaryOperator s13 = x -> x * cd;
        ToLongFunction<String> s14 = s -> s.length() + cl;
        Comparator<String> s15 = (a, b) -> a.length() - b.length();
        long acc = 0;
        for (int i = 0; i < N; i++) {
            acc += s0.getAsInt() + s1.applyAsInt(i, 3) + s2.f(i, i + 1L, 2);
            acc += (long) s3.f(i, 5L, 1.25f, 0.5, (i & 1) == 0 ? null : "z");
            acc += s4.f('a') + s5.f((byte) 100) + s6.f((short) 1000) + (s7.f((i & 1) == 0) ? 1 : 0);
            s8.f(i, (i & 1) == 0 ? null : "four");
            acc += String.valueOf(s9.f((i & 1) == 0 ? null : (Object) i)).length();
            acc += s10.get().length() + s11.apply(i) + s12.apply("p", i).length() + (long) s13.applyAsDouble(i) + s14.applyAsLong("abc") + s15.compare("aa", "b");
        }
        System.out.println("shapes acc=" + acc + " counter=" + counter);

        // 2. results of the narrowing returns
        System.out.println("narrow " + (int) s4.f('A') + " " + s5.f((byte) 127) + " " + s6.f((short) 32767) + " " + s7.f(false));

        // 3. checkcast replay: a generic lambda handed the wrong type must throw the same CCE
        @SuppressWarnings({"unchecked", "rawtypes"})
        Function raw = (Function<String, Integer>) str -> str.length();
        for (int i = 0; i < 50; i++) raw.apply("abc");
        run("cce", () -> raw.apply(5));
        run("cce2", () -> raw.apply(new int[2]));
        System.out.println("after cce " + raw.apply("hello"));

        // 4. exceptions thrown by the body: caught by the caller's handler, by an outer one, uncaught
        IntUnaryOperator thr = Sbr5InterpLambda::thrower;
        int caught = 0; long tot = 0;
        for (int i = 0; i < N; i++) {
            try { tot += thr.applyAsInt(i); } catch (IllegalStateException e) { caught++; if (i < 20) System.out.println("caught " + e.getMessage()); }
        }
        System.out.println("thr caught=" + caught + " tot=" + tot);
        IntUnaryOperator thr2 = x -> { try { return thrower(x); } catch (IllegalStateException e) { return -x; } finally { counter++; } };
        long t2 = 0; for (int i = 0; i < N; i++) t2 += thr2.applyAsInt(i);
        System.out.println("handler body " + t2 + " counter=" + counter);
        // the frames of an exception thrown through a lambda
        IntUnaryOperator thr3 = x -> thrower(3);
        for (int i = 0; i < 100; i++) { try { thr3.applyAsInt(0); } catch (IllegalStateException e) { if (i == 99) { StackTraceElement[] st = e.getStackTrace(); for (int k = 0; k < 3 && k < st.length; k++) System.out.println("  at " + st[k].getClassName() + "." + st[k].getMethodName()); } } }

        // 5. null receiver and null argument
        IntSupplier nul = null;
        run("npe", () -> nul.getAsInt());
        Function<String, Integer> lenF = x -> x.length();
        for (int i = 0; i < 50; i++) lenF.apply("a");
        run("npe-arg", () -> lenF.apply(null));

        // 6. polymorphic call site: the same bytecode site sees up to ten different proxy classes
        List<IntUnaryOperator> ops = new ArrayList<>();
        for (int k = 0; k < 10; k++) { final int kk = k; ops.add(x -> x + kk); }
        long poly = 0;
        for (int i = 0; i < N * 4; i++) poly += ops.get(i % 10).applyAsInt(i);
        System.out.println("poly " + poly);
        // and the same lambda expression evaluated many times (capturing: one proxy class, many instances)
        long inst = 0;
        for (int i = 0; i < N; i++) { final int c = i; IntSupplier sup = () -> c * 2; inst += sup.getAsInt() + sup.getAsInt(); }
        System.out.println("instances " + inst);

        // 7. method references: static, bound, unbound, constructor, interface default combinators
        IntBinaryOperator mr1 = Sbr5InterpLambda::staticTwo;
        Function<String, Integer> mr2 = Integer::parseInt;
        String bound = "bound!";
        IntSupplier mr3 = bound::length;
        Function<String, Integer> mr4 = String::length;
        Supplier<List<String>> mr5 = ArrayList::new;
        Sbr5InterpLambda self = new Sbr5InterpLambda();
        IntUnaryOperator mr6 = self::instMethod;
        Function<Integer, Integer> comp = s11.andThen(s11).compose(s11);
        Predicate<String> pr = ((Predicate<String>) String::isEmpty).negate().and(x -> x.length() < 5);
        long m = 0;
        for (int i = 0; i < N; i++) { m += mr1.applyAsInt(i, 2) + mr2.apply("12") + mr3.getAsInt() + mr4.apply("abc") + mr5.get().size() + mr6.applyAsInt(i) + comp.apply(i) + (pr.test("abc") ? 1 : 0); }
        System.out.println("methodrefs " + m);

        // 8. lambdas calling lambdas, and recursion (positive control)
        Rec[] fib = new Rec[1];
        fib[0] = n -> n < 2 ? n : fib[0].f(n - 1) + fib[0].f(n - 2);
        System.out.println("fib " + fib[0].f(18));
        deepLambda = n -> n == 0 ? 0 : 1 + deepLambda.f(n - 1);
        for (int i = 0; i < 100; i++) deepLambda.f(10);
        System.out.println("deep " + deepLambda.f(args.length > 0 && args[0].equals("deep") ? 4000 : 500));
        run("soe", () -> { Rec inf = new Rec() { public int f(int n) { return f(n + 1); } }; inf.f(0); });

        // 9. stateful lambda counting its own calls, a lambda in a static initializer, interleaving with threads
        int[] cnt = new int[1];
        Runnable counting = () -> cnt[0]++;
        for (int i = 0; i < N; i++) counting.run();
        System.out.println("cnt " + cnt[0]);
        Thread[] ts = new Thread[4];
        long[] part = new long[4];
        for (int t = 0; t < 4; t++) { final int tt = t; ts[t] = new Thread(() -> { IntUnaryOperator f = x -> x + tt; long a = 0; for (int i = 0; i < N * 5; i++) a += f.applyAsInt(i); part[tt] = a; }); ts[t].start(); }
        for (Thread t : ts) { try { t.join(); } catch (InterruptedException e) { } }
        System.out.println("threads " + Arrays.toString(part));
        System.out.println("sink " + sink);
    }
}
