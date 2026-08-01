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

    public static void main(String[] args) throws Throwable {
        lookupAndInvoke();
        adaptation();
        accessChecks();
        varHandles();
        System.out.println("CK RJdkHandles checks=" + checks);
        System.out.println("PASS RJdkHandles (" + checks + " checks)");
    }
}
