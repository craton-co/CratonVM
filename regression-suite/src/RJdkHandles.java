import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.invoke.WrongMethodTypeException;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * JDK-only corpus: {@code MethodHandle} / {@code VarHandle} -- lookup,
 * adaptation, access checks, atomics.
 *
 * The method-handle machinery is the substrate under lambdas, records,
 * string concatenation and reflection accessors, so a fabricated
 * {@code MethodHandles$Lookup} would take the whole corpus with it.
 *
 * Determinism: no addresses, no identity hashes, no timing.
 */
public class RJdkHandles {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static class Holder {
        private int i = 1;
        private long l = 2L;
        private String s = "s";
        private static int stat = 5;
        private final int[] arr = new int[] { 10, 20, 30 };
        // Narrow primitives, for the erased-shape checks below. All five of
        // int/short/byte/char/boolean travel as the same 32-bit value inside a
        // VM, so a wrapper chosen from the value alone rather than from the
        // field's declared type would box every one of them as Integer.
        private double d = 2.5;
        private boolean z = false;
        private byte b = 3;
        private short sh = 4;
        private char c = 'A';

        Holder() {
        }

        Holder(int i) {
            this.i = i;
        }

        private int secret(int add) {
            return i + add;
        }

        int pub(int add) {
            return i * add;
        }

        static int statAdd(int a, int b) {
            return a + b;
        }
    }

    static void lookupAndInvoke() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();

        MethodHandle stat = lk.findStatic(Holder.class, "statAdd",
                MethodType.methodType(int.class, int.class, int.class));
        check((int) stat.invokeExact(3, 4) == 7, "findStatic invokeExact");
        check((int) stat.invoke(3, 4) == 7, "findStatic invoke");
        check(stat.type().equals(MethodType.methodType(int.class, int.class, int.class)),
                "handle type");

        MethodHandle virt = lk.findVirtual(Holder.class, "pub",
                MethodType.methodType(int.class, int.class));
        Holder h = new Holder(6);
        check((int) virt.invokeExact(h, 7) == 42, "findVirtual invokeExact");

        // Private access is allowed because the lookup class is a nestmate.
        MethodHandle priv = lk.findVirtual(Holder.class, "secret",
                MethodType.methodType(int.class, int.class));
        check((int) priv.invokeExact(h, 1) == 7, "private nestmate access");

        MethodHandle ctor = lk.findConstructor(Holder.class,
                MethodType.methodType(void.class, int.class));
        Holder made = (Holder) ctor.invokeExact(99);
        check(made.i == 99, "findConstructor");

        MethodHandle getter = lk.findGetter(Holder.class, "s", String.class);
        check("s".equals((String) getter.invokeExact(h)), "findGetter");
        MethodHandle setter = lk.findSetter(Holder.class, "s", String.class);
        setter.invokeExact(h, "t");
        check("t".equals((String) getter.invokeExact(h)), "findSetter");
        MethodHandle sget = lk.findStaticGetter(Holder.class, "stat", int.class);
        check((int) sget.invokeExact() == 5, "findStaticGetter");

        // unreflect round-trips through core reflection.
        MethodHandle un = lk.unreflect(Holder.class.getDeclaredMethod("pub", int.class));
        check((int) un.invoke(h, 2) == 12, "unreflect");
        System.out.println("CK RJdkHandles invoke ok type=" + stat.type());
    }

    static void adaptation() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle add = lk.findStatic(Holder.class, "statAdd",
                MethodType.methodType(int.class, int.class, int.class));

        MethodHandle plus10 = MethodHandles.insertArguments(add, 0, 10);
        check((int) plus10.invokeExact(5) == 15, "insertArguments");

        MethodHandle dropped = MethodHandles.dropArguments(add, 0, String.class);
        check((int) dropped.invokeExact("ignored", 1, 2) == 3, "dropArguments");

        MethodHandle swapped = MethodHandles.permuteArguments(add,
                MethodType.methodType(int.class, int.class, int.class), 1, 0);
        check((int) swapped.invokeExact(1, 2) == 3, "permuteArguments");

        MethodHandle asObj = add.asType(
                MethodType.methodType(Integer.class, Integer.class, Integer.class));
        check(((Integer) asObj.invokeExact(Integer.valueOf(4), Integer.valueOf(5))) == 9,
                "asType boxing adapter");

        MethodHandle constant = MethodHandles.constant(String.class, "K");
        check("K".equals((String) constant.invokeExact()), "constant");
        MethodHandle ident = MethodHandles.identity(String.class);
        check("Z".equals((String) ident.invokeExact("Z")), "MethodHandles.identity");

        // filterArguments / foldArguments
        MethodHandle twice = lk.findStatic(RJdkHandles.class, "twice",
                MethodType.methodType(int.class, int.class));
        MethodHandle filtered = MethodHandles.filterArguments(add, 0, twice, twice);
        check((int) filtered.invokeExact(3, 4) == 14, "filterArguments");

        // guardWithTest picks a branch by predicate.
        MethodHandle isPos = lk.findStatic(RJdkHandles.class, "isPositive",
                MethodType.methodType(boolean.class, int.class));
        MethodHandle neg = lk.findStatic(RJdkHandles.class, "negate",
                MethodType.methodType(int.class, int.class));
        MethodHandle abs = MethodHandles.guardWithTest(isPos, MethodHandles.identity(int.class), neg);
        check((int) abs.invokeExact(5) == 5 && (int) abs.invokeExact(-5) == 5, "guardWithTest");

        // varargs collector
        MethodHandle sumAll = lk.findStatic(RJdkHandles.class, "sumAll",
                MethodType.methodType(int.class, int[].class)).asVarargsCollector(int[].class);
        check((int) sumAll.invoke(1, 2, 3, 4) == 10, "asVarargsCollector");
        MethodHandle spread = lk.findStatic(RJdkHandles.class, "sumAll",
                MethodType.methodType(int.class, int[].class))
                .asFixedArity();
        check((int) spread.invoke(new int[] { 5, 6 }) == 11, "asFixedArity");

        // arrayElementGetter / setter
        MethodHandle aget = MethodHandles.arrayElementGetter(int[].class);
        int[] a = { 7, 8, 9 };
        check((int) aget.invokeExact(a, 1) == 8, "arrayElementGetter");

        // A wrong invokeExact descriptor is a linkage-time error, not a silent coercion.
        boolean threw = false;
        try {
            long bogus = (long) add.invokeExact(1, 2);
            check(bogus == 3, "unreachable");
        } catch (WrongMethodTypeException expected) {
            threw = true;
        }
        check(threw, "invokeExact with the wrong descriptor must throw WrongMethodTypeException");
        System.out.println("CK RJdkHandles adapt=" + (int) filtered.invokeExact(3, 4));
    }

    static int twice(int x) {
        return x * 2;
    }

    static boolean isPositive(int x) {
        return x >= 0;
    }

    static int negate(int x) {
        return -x;
    }

    static int sumAll(int[] xs) {
        int s = 0;
        for (int x : xs) {
            s += x;
        }
        return s;
    }

    static void accessChecks() throws Throwable {
        // A public lookup cannot reach a private member.
        MethodHandles.Lookup pub = MethodHandles.publicLookup();
        boolean threw = false;
        try {
            pub.findVirtual(Holder.class, "secret", MethodType.methodType(int.class, int.class));
        } catch (IllegalAccessException expected) {
            threw = true;
        }
        check(threw, "publicLookup must not reach a private method");

        // ...nor can a lookup dropped to PUBLIC only.
        MethodHandles.Lookup dropped = MethodHandles.lookup()
                .dropLookupMode(MethodHandles.Lookup.PRIVATE);
        threw = false;
        try {
            dropped.findVirtual(Holder.class, "secret", MethodType.methodType(int.class, int.class));
        } catch (IllegalAccessException expected) {
            threw = true;
        }
        check(threw, "dropLookupMode(PRIVATE) must remove private access");

        // A missing member is NoSuchMethodException, not a fabricated handle.
        threw = false;
        try {
            MethodHandles.lookup().findStatic(Holder.class, "noSuchMethodAtAll",
                    MethodType.methodType(void.class));
        } catch (NoSuchMethodException expected) {
            threw = true;
        }
        check(threw, "missing method must raise NoSuchMethodException");
        threw = false;
        try {
            MethodHandles.lookup().findGetter(Holder.class, "noSuchField", int.class);
        } catch (NoSuchFieldException expected) {
            threw = true;
        }
        check(threw, "missing field must raise NoSuchFieldException");

        check((MethodHandles.lookup().lookupModes() & MethodHandles.Lookup.PRIVATE) != 0,
                "full lookup must have PRIVATE");
        check(MethodHandles.lookup().lookupClass() == RJdkHandles.class, "lookupClass");
        System.out.println("CK RJdkHandles accessChecks ok");
    }

    static void varHandles() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();

        VarHandle vi = lk.findVarHandle(Holder.class, "i", int.class);
        Holder h = new Holder();
        check((int) vi.get(h) == 1, "VarHandle get");
        vi.set(h, 5);
        check(h.i == 5, "VarHandle set");
        check(vi.compareAndSet(h, 5, 9), "VarHandle compareAndSet");
        check(!vi.compareAndSet(h, 5, 11), "VarHandle CAS must fail on a stale witness");
        check((int) vi.getAndAdd(h, 1) == 9 && h.i == 10, "VarHandle getAndAdd");
        check((int) vi.getAndSet(h, 3) == 10 && h.i == 3, "VarHandle getAndSet");
        check((int) vi.getVolatile(h) == 3, "getVolatile");
        vi.setVolatile(h, 4);
        check((int) vi.getAcquire(h) == 4, "getAcquire");
        vi.setRelease(h, 6);
        check((int) vi.getOpaque(h) == 6, "getOpaque");
        check((int) vi.compareAndExchange(h, 6, 7) == 6, "compareAndExchange");
        check(vi.varType() == int.class, "varType");
        check(vi.coordinateTypes().equals(List.of(Holder.class)), "coordinateTypes");

        VarHandle vl = lk.findVarHandle(Holder.class, "l", long.class);
        check((long) vl.getAndAdd(h, 40L) == 2L && h.l == 42L, "long VarHandle getAndAdd");

        VarHandle vs = lk.findVarHandle(Holder.class, "s", String.class);
        check(vs.compareAndSet(h, "s", "q"), "reference VarHandle CAS");
        check("q".equals(vs.get(h)), "reference VarHandle get");

        VarHandle vstat = lk.findStaticVarHandle(Holder.class, "stat", int.class);
        check((int) vstat.get() == 5, "static VarHandle get");
        vstat.set(5);

        // Array element VarHandle, including the bounds check.
        VarHandle va = MethodHandles.arrayElementVarHandle(int[].class);
        int[] arr = { 1, 2, 3 };
        check((int) va.getAndSet(arr, 1, 20) == 2 && arr[1] == 20, "array VarHandle getAndSet");
        check(va.compareAndSet(arr, 2, 3, 30) && arr[2] == 30, "array VarHandle CAS");
        boolean threw = false;
        try {
            va.get(arr, 7);
        } catch (ArrayIndexOutOfBoundsException expected) {
            threw = true;
        }
        check(threw, "array VarHandle must bounds-check");

        // Wrong-type access is a linkage error, not a coercion.
        threw = false;
        try {
            String bogus = (String) vi.get(h);
            check(bogus == null, "unreachable");
        } catch (WrongMethodTypeException | ClassCastException expected) {
            threw = true;
        }
        check(threw, "VarHandle wrong-type get must throw");

        // The same atomics through the public atomic classes, for cross-checking.
        AtomicInteger ai = new AtomicInteger(1);
        check(ai.compareAndSet(1, 2) && ai.get() == 2, "AtomicInteger CAS");
        check(ai.getAndIncrement() == 2 && ai.get() == 3, "AtomicInteger getAndIncrement");
        check(ai.accumulateAndGet(4, Integer::sum) == 7, "AtomicInteger accumulateAndGet");

        System.out.println("CK RJdkHandles varhandle i=" + h.i + " l=" + h.l + " s=" + vs.get(h)
                + " arr=" + Arrays.toString(arr) + " ai=" + ai.get());
    }

    /**
     * Assert one erased-shape access: the value arrives as a REFERENCE, it is
     * the wrapper the variable's declared type names, and it carries the right
     * payload.
     *
     * The wrapper-class assertion is not decoration. It is the half that a fix
     * applied at the wrong layer would fail: a VM that boxes a returned
     * primitive by inspecting the runtime value instead of the variable's type
     * cannot tell {@code boolean} from {@code int} and answers
     * {@code Integer(1)} where the JDK answers {@code Boolean.TRUE}.
     */
    static void erased(Object actual, Class<?> wrapper, Object expected, String what) {
        check(actual != null, what + ": erased call site must not yield null");
        check(actual.getClass() == wrapper,
                what + ": must box to " + wrapper.getName() + ", got " + actual.getClass().getName());
        check(expected.equals(actual), what + ": expected " + expected + ", got " + actual);
    }

    /**
     * Every {@code VarHandle} access mode that HANDS BACK the accessed
     * variable, at the ERASED call-site shape -- the result assigned to
     * {@code Object} rather than cast to the primitive.
     *
     * <p>Why this exists as a separate vector: {@code varHandles()} above uses
     * the cast form throughout, e.g. {@code (int) va.getAndSet(arr, 1, 20)}.
     * A signature-polymorphic call site takes its descriptor from the cast, so
     * that form links as {@code ([III)I} and exercises only the
     * primitive-return path. Drop the cast and the SAME call links as
     * {@code ([III)Ljava/lang/Object;} and must produce a box. The two shapes
     * are different code paths in a VM, and a suite that only ever writes the
     * convenient one cannot see a VM that gets the other wrong.
     *
     * <p>Deliberately covers all four coordinate families (array, instance
     * field, static field) and both value widths, plus every ordering variant,
     * because the modes are implemented one-per-function and a fix applied to
     * three of them is indistinguishable here from a fix applied to all.
     */
    static void varHandlesErased() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();

        // ---- int[] : read modes -------------------------------------------
        VarHandle va = MethodHandles.arrayElementVarHandle(int[].class);
        int[] a = { 1, 2, 3 };
        erased(va.get(a, 1), Integer.class, 2, "int[] get");
        erased(va.getVolatile(a, 1), Integer.class, 2, "int[] getVolatile");
        erased(va.getAcquire(a, 1), Integer.class, 2, "int[] getAcquire");
        erased(va.getOpaque(a, 1), Integer.class, 2, "int[] getOpaque");

        // ---- int[] : getAndSet and its ordering variants -------------------
        a = new int[] { 1, 2, 3 };
        erased(va.getAndSet(a, 1, 20), Integer.class, 2, "int[] getAndSet");
        check(a[1] == 20, "int[] getAndSet must still write");
        a = new int[] { 1, 2, 3 };
        erased(va.getAndSetAcquire(a, 1, 20), Integer.class, 2, "int[] getAndSetAcquire");
        a = new int[] { 1, 2, 3 };
        erased(va.getAndSetRelease(a, 1, 20), Integer.class, 2, "int[] getAndSetRelease");

        // ---- int[] : getAndAdd and its ordering variants -------------------
        a = new int[] { 1, 2, 3 };
        erased(va.getAndAdd(a, 1, 5), Integer.class, 2, "int[] getAndAdd");
        check(a[1] == 7, "int[] getAndAdd must still add");
        a = new int[] { 1, 2, 3 };
        erased(va.getAndAddAcquire(a, 1, 5), Integer.class, 2, "int[] getAndAddAcquire");
        a = new int[] { 1, 2, 3 };
        erased(va.getAndAddRelease(a, 1, 5), Integer.class, 2, "int[] getAndAddRelease");

        // ---- int[] : getAndBitwise{Or,And,Xor} and ordering variants -------
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseOr(a, 1, 1), Integer.class, 6, "int[] getAndBitwiseOr");
        check(a[1] == 7, "int[] getAndBitwiseOr must still or");
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseOrAcquire(a, 1, 1), Integer.class, 6, "int[] getAndBitwiseOrAcquire");
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseOrRelease(a, 1, 1), Integer.class, 6, "int[] getAndBitwiseOrRelease");
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseAnd(a, 1, 3), Integer.class, 6, "int[] getAndBitwiseAnd");
        check(a[1] == 2, "int[] getAndBitwiseAnd must still and");
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseAndAcquire(a, 1, 3), Integer.class, 6, "int[] getAndBitwiseAndAcquire");
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseAndRelease(a, 1, 3), Integer.class, 6, "int[] getAndBitwiseAndRelease");
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseXor(a, 1, 3), Integer.class, 6, "int[] getAndBitwiseXor");
        check(a[1] == 5, "int[] getAndBitwiseXor must still xor");
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseXorAcquire(a, 1, 3), Integer.class, 6, "int[] getAndBitwiseXorAcquire");
        a = new int[] { 1, 6, 3 };
        erased(va.getAndBitwiseXorRelease(a, 1, 3), Integer.class, 6, "int[] getAndBitwiseXorRelease");

        // ---- int[] : compareAndExchange, both witnesses --------------------
        a = new int[] { 1, 2, 3 };
        erased(va.compareAndExchange(a, 1, 2, 20), Integer.class, 2, "int[] compareAndExchange hit");
        check(a[1] == 20, "compareAndExchange must write on a matching witness");
        a = new int[] { 1, 2, 3 };
        erased(va.compareAndExchange(a, 1, 99, 20), Integer.class, 2, "int[] compareAndExchange miss");
        check(a[1] == 2, "compareAndExchange must not write on a stale witness");
        a = new int[] { 1, 2, 3 };
        erased(va.compareAndExchangeAcquire(a, 1, 2, 20), Integer.class, 2,
                "int[] compareAndExchangeAcquire");
        a = new int[] { 1, 2, 3 };
        erased(va.compareAndExchangeRelease(a, 1, 2, 20), Integer.class, 2,
                "int[] compareAndExchangeRelease");

        // ---- long[] / double[] : the two-slot widths -----------------------
        VarHandle vla = MethodHandles.arrayElementVarHandle(long[].class);
        long[] la = { 1L, 2L, 3L };
        erased(vla.get(la, 1), Long.class, 2L, "long[] get");
        la = new long[] { 1L, 2L, 3L };
        erased(vla.getAndSet(la, 1, 20L), Long.class, 2L, "long[] getAndSet");
        la = new long[] { 1L, 2L, 3L };
        erased(vla.getAndAdd(la, 1, 5L), Long.class, 2L, "long[] getAndAdd");
        la = new long[] { 1L, 6L, 3L };
        erased(vla.getAndBitwiseOr(la, 1, 1L), Long.class, 6L, "long[] getAndBitwiseOr");
        la = new long[] { 1L, 2L, 3L };
        erased(vla.compareAndExchange(la, 1, 2L, 20L), Long.class, 2L, "long[] compareAndExchange");

        VarHandle vda = MethodHandles.arrayElementVarHandle(double[].class);
        double[] da = { 1.5, 2.5, 3.5 };
        erased(vda.get(da, 1), Double.class, 2.5, "double[] get");
        da = new double[] { 1.5, 2.5, 3.5 };
        erased(vda.getAndSet(da, 1, 20.25), Double.class, 2.5, "double[] getAndSet");
        da = new double[] { 1.5, 2.5, 3.5 };
        erased(vda.getAndAdd(da, 1, 5.0), Double.class, 2.5, "double[] getAndAdd");
        da = new double[] { 1.5, 2.5, 3.5 };
        erased(vda.compareAndExchange(da, 1, 2.5, 20.25), Double.class, 2.5,
                "double[] compareAndExchange");

        // ---- boolean[] : the wrapper a value-shaped fix gets wrong ---------
        VarHandle vza = MethodHandles.arrayElementVarHandle(boolean[].class);
        boolean[] za = { false, true, false };
        erased(vza.get(za, 1), Boolean.class, Boolean.TRUE, "boolean[] get");
        za = new boolean[] { false, true, false };
        erased(vza.getAndSet(za, 1, false), Boolean.class, Boolean.TRUE, "boolean[] getAndSet");
        check(!za[1], "boolean[] getAndSet must still write");
        za = new boolean[] { false, true, false };
        erased(vza.getAndBitwiseOr(za, 1, false), Boolean.class, Boolean.TRUE,
                "boolean[] getAndBitwiseOr");
        za = new boolean[] { false, true, false };
        erased(vza.getAndBitwiseAnd(za, 1, false), Boolean.class, Boolean.TRUE,
                "boolean[] getAndBitwiseAnd");
        za = new boolean[] { false, true, false };
        erased(vza.getAndBitwiseXor(za, 1, true), Boolean.class, Boolean.TRUE,
                "boolean[] getAndBitwiseXor");
        za = new boolean[] { false, true, false };
        erased(vza.compareAndExchange(za, 1, true, false), Boolean.class, Boolean.TRUE,
                "boolean[] compareAndExchange");

        // ---- String[] : a reference variable must NOT be re-wrapped --------
        VarHandle vsa = MethodHandles.arrayElementVarHandle(String[].class);
        String[] sa = { "x", "y", "z" };
        erased(vsa.get(sa, 1), String.class, "y", "String[] get");
        sa = new String[] { "x", "y", "z" };
        erased(vsa.getAndSet(sa, 1, "Y"), String.class, "y", "String[] getAndSet");
        check("Y".equals(sa[1]), "String[] getAndSet must still write");
        sa = new String[] { "x", "y", "z" };
        erased(vsa.compareAndExchange(sa, 1, "y", "Y"), String.class, "y",
                "String[] compareAndExchange hit");
        sa = new String[] { "x", "y", "z" };
        erased(vsa.compareAndExchange(sa, 1, "q", "Y"), String.class, "y",
                "String[] compareAndExchange miss");
        check("y".equals(sa[1]), "String[] compareAndExchange must not write on a stale witness");

        // ---- instance-field VarHandles (findVarHandle) ---------------------
        VarHandle fi = lk.findVarHandle(Holder.class, "i", int.class);
        Holder h = new Holder();
        erased(fi.get(h), Integer.class, 1, "field int get");
        erased(fi.getVolatile(h), Integer.class, 1, "field int getVolatile");
        erased(fi.getAcquire(h), Integer.class, 1, "field int getAcquire");
        erased(fi.getOpaque(h), Integer.class, 1, "field int getOpaque");
        h = new Holder();
        erased(fi.getAndSet(h, 20), Integer.class, 1, "field int getAndSet");
        check(h.i == 20, "field getAndSet must still write");
        h = new Holder();
        erased(fi.getAndSetAcquire(h, 20), Integer.class, 1, "field int getAndSetAcquire");
        h = new Holder();
        erased(fi.getAndSetRelease(h, 20), Integer.class, 1, "field int getAndSetRelease");
        h = new Holder();
        erased(fi.getAndAdd(h, 5), Integer.class, 1, "field int getAndAdd");
        check(h.i == 6, "field getAndAdd must still add");
        h = new Holder();
        erased(fi.getAndAddAcquire(h, 5), Integer.class, 1, "field int getAndAddAcquire");
        h = new Holder();
        erased(fi.getAndAddRelease(h, 5), Integer.class, 1, "field int getAndAddRelease");
        h = new Holder();
        h.i = 6;
        erased(fi.getAndBitwiseOr(h, 1), Integer.class, 6, "field int getAndBitwiseOr");
        h = new Holder();
        h.i = 6;
        erased(fi.getAndBitwiseAnd(h, 3), Integer.class, 6, "field int getAndBitwiseAnd");
        h = new Holder();
        h.i = 6;
        erased(fi.getAndBitwiseXor(h, 3), Integer.class, 6, "field int getAndBitwiseXor");
        h = new Holder();
        erased(fi.compareAndExchange(h, 1, 20), Integer.class, 1, "field int compareAndExchange hit");
        check(h.i == 20, "field compareAndExchange must write on a matching witness");
        h = new Holder();
        erased(fi.compareAndExchange(h, 99, 20), Integer.class, 1, "field int compareAndExchange miss");
        check(h.i == 1, "field compareAndExchange must not write on a stale witness");

        // The narrow primitives: each is a distinct wrapper the JDK picks from
        // the FIELD's declared type, not from the value.
        h = new Holder();
        erased(lk.findVarHandle(Holder.class, "l", long.class).getAndAdd(h, 40L),
                Long.class, 2L, "field long getAndAdd");
        h = new Holder();
        erased(lk.findVarHandle(Holder.class, "d", double.class).getAndAdd(h, 1.25),
                Double.class, 2.5, "field double getAndAdd");
        h = new Holder();
        erased(lk.findVarHandle(Holder.class, "z", boolean.class).getAndSet(h, true),
                Boolean.class, Boolean.FALSE, "field boolean getAndSet");
        check(h.z, "field boolean getAndSet must still write");
        h = new Holder();
        erased(lk.findVarHandle(Holder.class, "b", byte.class).getAndAdd(h, (byte) 2),
                Byte.class, (byte) 3, "field byte getAndAdd");
        h = new Holder();
        erased(lk.findVarHandle(Holder.class, "sh", short.class).getAndAdd(h, (short) 2),
                Short.class, (short) 4, "field short getAndAdd");
        h = new Holder();
        erased(lk.findVarHandle(Holder.class, "c", char.class).getAndSet(h, 'B'),
                Character.class, 'A', "field char getAndSet");
        h = new Holder();
        erased(lk.findVarHandle(Holder.class, "s", String.class).getAndSet(h, "t"),
                String.class, "s", "field String getAndSet");
        h = new Holder();
        erased(lk.findVarHandle(Holder.class, "s", String.class).compareAndExchange(h, "s", "t"),
                String.class, "s", "field String compareAndExchange");

        // ---- static-field VarHandle ----------------------------------------
        VarHandle vstat = lk.findStaticVarHandle(Holder.class, "stat", int.class);
        Holder.stat = 5;
        erased(vstat.get(), Integer.class, 5, "static get");
        Holder.stat = 5;
        erased(vstat.getAndSet(7), Integer.class, 5, "static getAndSet");
        check(Holder.stat == 7, "static getAndSet must still write");
        Holder.stat = 5;
        erased(vstat.getAndAdd(7), Integer.class, 5, "static getAndAdd");
        Holder.stat = 5;
        erased(vstat.getAndBitwiseOr(2), Integer.class, 5, "static getAndBitwiseOr");
        Holder.stat = 5;
        erased(vstat.compareAndExchange(5, 7), Integer.class, 5, "static compareAndExchange");
        Holder.stat = 5;

        // The exact shape the defect was first seen in: no cast anywhere, the
        // result printed through println(Object).
        int[] w = { 1, 2, 3 };
        System.out.println("CK RJdkHandles erased getAndSet=" + va.getAndSet(w, 1, 20)
                + " arr=" + Arrays.toString(w));
    }

    public static void main(String[] args) throws Throwable {
        lookupAndInvoke();
        adaptation();
        accessChecks();
        varHandles();
        varHandlesErased();
        System.out.println("CK RJdkHandles checks=" + checks);
        System.out.println("PASS RJdkHandles (" + checks + " checks)");
    }
}
