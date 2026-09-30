import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.invoke.WrongMethodTypeException;
import java.lang.reflect.Method;
import java.util.ArrayList;
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
 *
 * <h2>Why this class is built out of {@link #step steps}</h2>
 *
 * It used to be one straight-line run of {@code check(...)} calls, and that
 * made it a ONE-BIT instrument over a TEN-COMBINATOR surface. When every
 * combinator carrier in {@code lang_invoke.rs} was being refused under
 * {@code --jdk-only}, this vector reported exactly one failure --
 * {@code NoClassDefFoundError: __mh_insert_wrapper__} at the first assertion of
 * {@code adaptation()} -- because the throw ended the run. A scratch probe that
 * reached each combinator independently, catching per step, named all ten in a
 * single run. Ten broken combinators, one reported.
 *
 * <p>So every independent claim here runs inside {@link #step}: the failure is
 * caught, PRINTED WITH ITS OWN NAME, recorded, and the next claim still runs.
 * {@code main} throws at the end if anything failed, so the exit code and the
 * suite's {@code rc} check are unchanged -- what changes is that one run now
 * names every broken combinator instead of the first.
 *
 * <p>Two rules from {@code W6-5-vacuous-tests.md} govern what may be added:
 * every check must be one that could be made to FAIL by breaking the VM, and a
 * check that cannot fail must not be written. Concretely, that is why several
 * checks here are shaped the way they are:
 *
 * <ul>
 *   <li>{@code permuteArguments} is asserted on a NON-COMMUTATIVE target. The
 *       old assertion permuted {@code statAdd(int,int)} and demanded 3 from
 *       {@code (1,2)} -- which a permutation that did nothing at all also
 *       produces. It could not fail.</li>
 *   <li>{@code catchException} carries a negative control: a target that does
 *       NOT throw must return the target's value, not the handler's. Without
 *       it, a VM that ran the handler unconditionally would pass.</li>
 *   <li>{@code bindTo} carries both halves: a leading REFERENCE parameter must
 *       be accepted and a leading PRIMITIVE one refused. Without the first
 *       half, a VM that refused every {@code bindTo} would pass.</li>
 *   <li>{@code asCollector} asserts the runtime CLASS of the array it built,
 *       not only the value that came back out of it. The value can be right for
 *       the wrong reason; {@code [I} cannot.</li>
 * </ul>
 */
public class RJdkHandles {
    static int checks;
    static int steps;
    static final List<String> failures = new ArrayList<>();

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    interface Body {
        void run() throws Throwable;
    }

    /**
     * Run one INDEPENDENT claim. A failure is named, recorded and survived; the
     * next claim still runs.
     *
     * <p>The printed line is deliberately not a {@code CK } line: {@code run.sh}
     * keeps only {@code PASS}/{@code CK} lines for its cross-VM diff, and a
     * failing run is already red on {@code rc} before that diff is reached. The
     * line is here for whoever runs the class directly, which is the workflow
     * that has to distinguish "one carrier broken" from "ten".
     */
    static void step(String name, Body body) {
        steps++;
        try {
            body.run();
        } catch (Throwable t) {
            failures.add(name);
            System.out.println("FAIL RJdkHandles step " + name + ": "
                    + t.getClass().getName() + ": " + t.getMessage());
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

    static MethodHandle st(String name, Class<?> ret, Class<?>... params) throws Throwable {
        return MethodHandles.lookup().findStatic(RJdkHandles.class,
                name, MethodType.methodType(ret, params));
    }

    /**
     * The combinator surface, ONE INDEPENDENT STEP PER COMBINATOR.
     *
     * <p>Each step builds its own target handle rather than sharing one. That
     * is the point: a shared setup line that fails takes every later claim with
     * it, which is precisely the one-bit behaviour being removed here. The cost
     * is a few extra {@code findStatic} calls; the benefit is that a green run
     * means every combinator below is green.
     *
     * <p>Every value asserted here was measured on HotSpot 25.0.3 before it was
     * written down.
     */
    static void adaptation() throws Throwable {
        step("insertArguments", () -> {
            MethodHandle add = st("plus", int.class, int.class, int.class);
            // Bound at position 0, so the surviving parameter is the SECOND
            // one. A splice at the wrong end still answers 15 for a commutative
            // target, so the target is `plus` and the check below is the
            // non-commutative one.
            check((int) MethodHandles.insertArguments(add, 0, 10).invokeExact(5) == 15,
                    "insertArguments at 0");
            MethodHandle minus = st("minus", int.class, int.class, int.class);
            // minus(10, x) at pos 0 -> 10-4 = 6; a splice at pos 1 would be
            // minus(4, 10) = -6, so this distinguishes the two.
            check((int) MethodHandles.insertArguments(minus, 0, 10).invokeExact(4) == 6,
                    "insertArguments at 0 binds the FIRST parameter");
            check((int) MethodHandles.insertArguments(minus, 1, 10).invokeExact(4) == -6,
                    "insertArguments at 1 binds the SECOND parameter");
        });

        step("dropArguments", () -> {
            MethodHandle add = st("plus", int.class, int.class, int.class);
            MethodHandle dropped = MethodHandles.dropArguments(add, 0, String.class);
            check((int) dropped.invokeExact("ignored", 1, 2) == 3, "dropArguments at 0");
            // Dropping in the MIDDLE, which a "skip the first N" implementation
            // gets wrong: the ignored argument sits between 1 and 2.
            MethodHandle minus = st("minus", int.class, int.class, int.class);
            MethodHandle mid = MethodHandles.dropArguments(minus, 1, String.class);
            check((int) mid.invokeExact(9, "ignored", 4) == 5, "dropArguments at 1");
        });

        step("permuteArguments", () -> {
            // NON-COMMUTATIVE on purpose. The predecessor of this check
            // permuted `statAdd` and demanded 3 from (1,2) -- a permutation
            // that did nothing produces 3 as well, so it could not fail. `minus`
            // separates the two: swapped gives 2-1 = 1, unswapped gives -1.
            MethodHandle minus = st("minus", int.class, int.class, int.class);
            MethodHandle swapped = MethodHandles.permuteArguments(minus,
                    MethodType.methodType(int.class, int.class, int.class), 1, 0);
            check((int) swapped.invokeExact(1, 2) == 1, "permuteArguments swaps");
            // A permutation may also DUPLICATE and DROP: (a,b) -> minus(b,b).
            MethodHandle dup = MethodHandles.permuteArguments(minus,
                    MethodType.methodType(int.class, int.class, int.class), 1, 1);
            check((int) dup.invokeExact(7, 4) == 0, "permuteArguments duplicates and drops");
        });

        step("filterArguments", () -> {
            MethodHandle minus = st("minus", int.class, int.class, int.class);
            MethodHandle twice = st("twice", int.class, int.class);
            // Filter only the SECOND argument: minus(9, 2*4) = 1. Filtering
            // both would give 18-8 = 10 and filtering none 5, so this pins the
            // position as well as the fact that a filter ran.
            MethodHandle f1 = MethodHandles.filterArguments(minus, 1, twice);
            check((int) f1.invokeExact(9, 4) == 1, "filterArguments at 1");
            MethodHandle f2 = MethodHandles.filterArguments(minus, 0, twice, twice);
            check((int) f2.invokeExact(9, 4) == 10, "filterArguments at 0, both");
        });

        step("guardWithTest", () -> {
            MethodHandle isPos = st("isPositive", boolean.class, int.class);
            MethodHandle neg = st("negate", int.class, int.class);
            MethodHandle abs = MethodHandles.guardWithTest(isPos,
                    MethodHandles.identity(int.class), neg);
            // BOTH branches, so a guard wired to a constant answer fails one of
            // them.
            check((int) abs.invokeExact(5) == 5, "guardWithTest takes the TRUE branch");
            check((int) abs.invokeExact(-5) == 5, "guardWithTest takes the FALSE branch");
        });

        step("filterReturnValue", () -> {
            MethodHandle add = st("plus", int.class, int.class, int.class);
            MethodHandle negate = st("negate", int.class, int.class);
            // -7, not 7: an unfiltered return is a different value, so a
            // pass-through filterReturnValue cannot satisfy this.
            check((int) MethodHandles.filterReturnValue(add, negate).invokeExact(3, 4) == -7,
                    "filterReturnValue");
        });

        step("foldArguments", () -> {
            MethodHandle minus = st("minus", int.class, int.class, int.class);
            MethodHandle twice = st("twice", int.class, int.class);
            // fold PREPENDS the combiner's result and KEEPS the original
            // arguments: minus(twice(5), 5) = 5. `collectArguments` REPLACES
            // instead, and the step below asserts that difference.
            check((int) MethodHandles.foldArguments(minus, twice).invokeExact(5) == 5,
                    "foldArguments prepends and keeps");
        });

        step("collectArguments", () -> {
            MethodHandle minus = st("minus", int.class, int.class, int.class);
            MethodHandle twice = st("twice", int.class, int.class);
            // collect REPLACES parameter 0 with the combiner's result:
            // minus(twice(3), 4) = 2. Fold on the same handles would need a
            // third argument, so the two cannot be confused by value.
            check((int) MethodHandles.collectArguments(minus, 0, twice).invokeExact(3, 4) == 2,
                    "collectArguments at 0");
            check((int) MethodHandles.collectArguments(minus, 1, twice).invokeExact(9, 3) == 3,
                    "collectArguments at 1");
        });

        step("catchException", () -> {
            MethodHandle boom = st("boom", int.class, int.class);
            MethodHandle handler = st("recover", int.class,
                    IllegalStateException.class, int.class);
            check((int) MethodHandles.catchException(boom,
                    IllegalStateException.class, handler).invokeExact(7) == 1007,
                    "catchException runs the handler on a throw");
            // NEGATIVE CONTROL. Without this a VM that ran the handler
            // unconditionally -- or one that never invoked the target at all --
            // passes the line above.
            MethodHandle quiet = st("twice", int.class, int.class);
            check((int) MethodHandles.catchException(quiet,
                    IllegalStateException.class, handler).invokeExact(7) == 14,
                    "catchException must NOT run the handler when the target returns");
            // ...and the guarded type must be honoured: a throwable outside it
            // propagates rather than being swallowed by the handler.
            MethodHandle other = st("boomOther", int.class, int.class);
            boolean propagated = false;
            try {
                int unreachable = (int) MethodHandles.catchException(other,
                        IllegalStateException.class, handler).invokeExact(7);
                check(unreachable == -1, "unreachable");
            } catch (IllegalArgumentException expected) {
                propagated = true;
            }
            check(propagated, "catchException must not catch a type it was not given");
        });

        step("asType boxing adapter", () -> {
            MethodHandle add = st("plus", int.class, int.class, int.class);
            MethodHandle asObj = add.asType(
                    MethodType.methodType(Integer.class, Integer.class, Integer.class));
            check(((Integer) asObj.invokeExact(Integer.valueOf(4), Integer.valueOf(5))) == 9,
                    "asType boxing adapter");
        });

        step("constant and identity", () -> {
            MethodHandle constant = MethodHandles.constant(String.class, "K");
            check("K".equals((String) constant.invokeExact()), "constant");
            MethodHandle ident = MethodHandles.identity(String.class);
            check("Z".equals((String) ident.invokeExact("Z")), "MethodHandles.identity");
        });

        step("arrayElementGetter", () -> {
            MethodHandle aget = MethodHandles.arrayElementGetter(int[].class);
            int[] a = { 7, 8, 9 };
            check((int) aget.invokeExact(a, 1) == 8, "arrayElementGetter");
        });

        step("invokeExact descriptor mismatch", () -> {
            // A wrong invokeExact descriptor is a linkage-time error, not a
            // silent coercion.
            MethodHandle add = st("plus", int.class, int.class, int.class);
            boolean threw = false;
            try {
                long bogus = (long) add.invokeExact(1, 2);
                check(bogus == 3, "unreachable");
            } catch (WrongMethodTypeException expected) {
                threw = true;
            }
            check(threw,
                    "invokeExact with the wrong descriptor must throw WrongMethodTypeException");
        });

        System.out.println("CK RJdkHandles adapt combinators=" + steps);
    }

    /**
     * {@code asCollector} once per ARRAY CARRIER, plus the spreader family.
     *
     * <p>This exists because {@code asCollector} answered a wrong value for
     * every PRIMITIVE array type while answering correctly for {@code Object[]}
     * and {@code String[]}: the collect arm gathered into a reference array
     * unconditionally, so the target's {@code iaload}/{@code laload} read an oop
     * as a value. Measured on the shipped binary under {@code --real-jdk}:
     * {@code int[]}&rarr;0, {@code long[]}&rarr;-2527743864898872 (a raw heap
     * pointer), {@code double[]}&rarr;NaN, against HotSpot's 6, 6 and 7.0.
     *
     * <p>The vector could not see it: {@code adaptation()} only ever reached
     * {@code asCollector} through {@code asVarargsCollector}, which takes a
     * different path and was green. One reachable carrier is not the surface --
     * so every carrier is reached here, one step each.
     *
     * <p>Each carrier asserts the RUNTIME CLASS of the array that was built as
     * well as the value computed from it. The value alone is the weaker half:
     * a container that is wrong but whose elements unbox correctly on the way
     * out could still produce 6. {@code "[I"} could not.
     */
    static void collectors() throws Throwable {
        step("asCollector int[]", () -> {
            check((int) st("sumInts", int.class, int[].class)
                    .asCollector(int[].class, 3).invoke(1, 2, 3) == 6, "asCollector int[] value");
            check("[I".equals((String) st("classOfInts", String.class, int[].class)
                    .asCollector(int[].class, 3).invoke(1, 2, 3)),
                    "asCollector int[] must build an int[]");
        });
        step("asCollector long[]", () -> {
            check((long) st("sumLongs", long.class, long[].class)
                    .asCollector(long[].class, 3).invoke(1L, 2L, 3L) == 6L,
                    "asCollector long[] value");
            check("[J".equals((String) st("classOfLongs", String.class, long[].class)
                    .asCollector(long[].class, 3).invoke(1L, 2L, 3L)),
                    "asCollector long[] must build a long[]");
        });
        step("asCollector double[]", () -> check((double) st("sumDoubles", double.class,
                double[].class).asCollector(double[].class, 3).invoke(1.5, 2.5, 3.0) == 7.0,
                "asCollector double[] value"));
        step("asCollector float[]", () -> check((float) st("sumFloats", float.class,
                float[].class).asCollector(float[].class, 2).invoke(1.5f, 2.25f) == 3.75f,
                "asCollector float[] value"));
        step("asCollector byte[]", () -> check((int) st("sumBytes", int.class, byte[].class)
                .asCollector(byte[].class, 3).invoke((byte) 1, (byte) 2, (byte) 3) == 6,
                "asCollector byte[] value"));
        step("asCollector short[]", () -> check((int) st("sumShorts", int.class, short[].class)
                .asCollector(short[].class, 3).invoke((short) 10, (short) 20, (short) 30) == 60,
                "asCollector short[] value"));
        step("asCollector char[]", () -> check((int) st("sumChars", int.class, char[].class)
                .asCollector(char[].class, 2).invoke('A', 'B') == 131,
                "asCollector char[] value"));
        step("asCollector boolean[]", () -> check((int) st("countTrue", int.class, boolean[].class)
                .asCollector(boolean[].class, 3).invoke(true, false, true) == 2,
                "asCollector boolean[] value"));
        step("asCollector String[]", () -> {
            // The two carriers that were already right before the fix, kept so
            // the fix cannot trade them away: a change that made every collector
            // primitive would break exactly here.
            check("abc".equals((String) st("catStrings", String.class, String[].class)
                    .asCollector(String[].class, 3).invoke("a", "b", "c")),
                    "asCollector String[] value");
            check("[Ljava.lang.String;".equals((String) st("classOfStrings", String.class,
                    String[].class).asCollector(String[].class, 3).invoke("a", "b", "c")),
                    "asCollector String[] must build a String[], not an Object[]");
        });
        step("asCollector Object[]", () -> {
            check((int) st("countObjects", int.class, Object[].class)
                    .asCollector(Object[].class, 2).invoke("x", "y") == 2,
                    "asCollector Object[] value");
            // A primitive handed to a REFERENCE collector must still be boxed:
            // this is the Groovy indy shape (`invoke(II)Object`), and the
            // element's class is the only way to see the boxing happened.
            check("java.lang.Integer".equals((String) st("classOfFirst", String.class,
                    Object[].class).asCollector(Object[].class, 2).invoke(1, 2)),
                    "asCollector Object[] must box a primitive element");
        });
        step("asCollector type()", () -> {
            // Independent of the value: the ARITY bookkeeping was already right
            // when the container was wrong, so this is not the gate for that
            // defect and is not written as if it were.
            MethodHandle c = st("sumInts", int.class, int[].class).asCollector(int[].class, 3);
            check("(int,int,int)int".equals(c.type().toString()), "asCollector type()");
        });
        step("asCollector with a leading argument", () -> {
            // The collector's trailing-args split: only the LAST two arguments
            // are gathered, the leading String is passed straight through.
            MethodHandle c = st("labelled", String.class, String.class, int[].class)
                    .asCollector(int[].class, 2);
            check("n=3".equals((String) c.invoke("n", 1, 2)), "asCollector leading argument");
        });

        step("asSpreader", () -> {
            MethodHandle add = st("plus", int.class, int.class, int.class);
            check((int) add.asSpreader(int[].class, 2).invokeExact(new int[] { 3, 4 }) == 7,
                    "asSpreader int[]");
            MethodHandle cat = st("catTwo", String.class, String.class, String.class);
            check("ab".equals((String) cat.asSpreader(String[].class, 2)
                    .invokeExact(new String[] { "a", "b" })), "asSpreader String[]");
        });
        step("asVarargsCollector", () -> {
            MethodHandle sumAll = st("sumInts", int.class, int[].class)
                    .asVarargsCollector(int[].class);
            check((int) sumAll.invoke(1, 2, 3, 4) == 10, "asVarargsCollector spread call");
            // The same handle must still accept the array form -- that is what
            // makes it VARIABLE arity rather than a collector.
            check((int) sumAll.invoke(new int[] { 5, 6 }) == 11,
                    "asVarargsCollector array call");
            // The MARKING. This answered `false` on CratonVM until W7-19 5.2:
            // `asVarargsCollector` was the identity and stored the marking
            // nowhere, so `isVarargsCollector()` fell through to the base
            // class's `return false`. It is now the sixth synthetic slot.
            check(sumAll.isVarargsCollector(),
                    "asVarargsCollector's result must report isVarargsCollector()");
            // The control that keeps the line above from being satisfied by a
            // VM that answers `true` unconditionally. It must be a FRESH handle:
            // CratonVM's `asVarargsCollector` is still the identity, so marking
            // is visible through the receiver too (a declared deviation -- see
            // W7-19 5.2), and re-reading the receiver of the call above would
            // assert that deviation rather than the marking.
            check(!st("sumInts", int.class, int[].class).isVarargsCollector(),
                    "a plain findStatic handle is not a varargs collector");
        });
        step("asFixedArity", () -> {
            MethodHandle fixed = st("sumInts", int.class, int[].class)
                    .asVarargsCollector(int[].class).asFixedArity();
            check((int) fixed.invoke(new int[] { 5, 6 }) == 11, "asFixedArity array call");
            // Now a real control rather than half of one. While the flag was
            // hardcoded `false` this passed because it was ALWAYS false, which
            // is why W7-19 4.2 deleted it; with the marking stored, it
            // distinguishes "asFixedArity cleared it" from "always true".
            check(!fixed.isVarargsCollector(),
                    "asFixedArity's result must not report isVarargsCollector()");
        });
    }

    /**
     * {@code bindTo}: what it must ACCEPT and what it must REFUSE.
     *
     * <p>The refusal is the half CratonVM did not have.
     * {@code MethodHandle.bindTo}'s javadoc: <em>"@throws
     * IllegalArgumentException if the target does not have a leading parameter
     * type that is a reference type"</em>, implemented in
     * {@code MethodType.leadingReferenceParameter()} as
     * {@code if (ptypes.length == 0 || ptypes[0].isPrimitive()) throw ...}.
     * Measured on the shipped binary under {@code --real-jdk}: all three
     * refusals below were ACCEPTED, and the first answered 6.
     *
     * <p>The accepting checks are not decoration. Without them a VM that threw
     * {@code IllegalArgumentException} from every {@code bindTo} passes the
     * refusals and is indistinguishable from a correct one.
     */
    static void binding() throws Throwable {
        step("bindTo accepts a leading reference parameter", () -> {
            MethodHandle m = st("labelledTimes", int.class, String.class, int.class);
            check((int) m.bindTo("abc").invoke(2) == 6, "bindTo(String) binds and invokes");
            check("(int)int".equals(m.bindTo("abc").type().toString()),
                    "bindTo drops the bound leading parameter from type()");
            // A leading ARRAY parameter is a reference parameter too.
            MethodHandle s = st("sumInts", int.class, int[].class);
            check((int) s.bindTo(new int[] { 4, 5 }).invoke() == 9, "bindTo(int[])");
        });
        step("bindTo accepts null for a reference parameter", () -> {
            // Measured on HotSpot 25: null is a legal bind, and the type is
            // still narrowed. A refusal implemented as a null check rather than
            // a TYPE check fails here.
            MethodHandle m = st("labelledTimes", int.class, String.class, int.class);
            check("(int)int".equals(m.bindTo(null).type().toString()), "bindTo(null)");
        });
        step("bindTo refuses a leading int parameter", () -> {
            MethodHandle m = st("labelledTimes", int.class, String.class, int.class);
            MethodHandle bound = m.bindTo("abc");   // now (int)int
            boolean threw = false;
            try {
                MethodHandle again = bound.bindTo(2);
                check(false, "bindTo(int) was ACCEPTED and produced " + again.type());
            } catch (IllegalArgumentException expected) {
                threw = true;
            }
            check(threw, "bindTo on a leading int must raise IllegalArgumentException");
        });
        step("bindTo refuses a leading long parameter", () -> {
            MethodHandle m = st("twiceLong", long.class, long.class);
            boolean threw = false;
            try {
                MethodHandle again = m.bindTo(5L);
                check(false, "bindTo(long) was ACCEPTED and produced " + again.type());
            } catch (IllegalArgumentException expected) {
                threw = true;
            }
            check(threw, "bindTo on a leading long must raise IllegalArgumentException");
        });
        step("bindTo refuses a zero-arity target", () -> {
            // `ptypes.length == 0` is the other half of the JDK's test, and it
            // is a distinct code path from the primitive test.
            MethodHandle k = MethodHandles.constant(String.class, "K");
            boolean threw = false;
            try {
                MethodHandle again = k.bindTo("x");
                check(false, "bindTo on ()String was ACCEPTED and produced " + again.type());
            } catch (IllegalArgumentException expected) {
                threw = true;
            }
            check(threw, "bindTo on a zero-arity target must raise IllegalArgumentException");
        });
        // W7-19 5.3. The `type()` bookkeeping after a bind was correct for
        // static/virtual/special handles and MISSING for the four accessor
        // kinds. Measured on the shipped binary against HotSpot 25, same class
        // file: a bound `findGetter` reported `(Holder)int` where HotSpot
        // reports `()int`, and a bound `arrayElementGetter` reported
        // `([I,int)int` where HotSpot reports `(int)int`.
        //
        // Asserted through `parameterCount`/`parameterType` rather than
        // `MethodType.toString()`: the arity is the claim, and a rendering
        // difference in some unrelated change must not fail this.
        step("bindTo narrows type() after a getter bind", () -> {
            MethodHandle g = MethodHandles.lookup().findGetter(Holder.class, "i", int.class);
            check(g.type().parameterCount() == 1,
                    "an unbound instance getter takes the receiver");
            MethodHandle bg = g.bindTo(new Holder(11));
            check(bg.type().parameterCount() == 0,
                    "a bound getter must drop the receiver from type()");
            check(bg.type().returnType() == int.class, "a bound getter still returns int");
            check((int) bg.invoke() == 11, "a bound getter reads the bound receiver's field");
        });
        step("bindTo narrows type() after an array-element getter bind", () -> {
            MethodHandle ag = MethodHandles.arrayElementGetter(int[].class);
            check(ag.type().parameterCount() == 2,
                    "an unbound array-element getter takes (array, index)");
            MethodHandle bag = ag.bindTo(new int[] { 7, 8, 9 });
            check(bag.type().parameterCount() == 1,
                    "a bound array-element getter must drop the array from type()");
            check(bag.type().parameterType(0) == int.class,
                    "a bound array-element getter takes the index");
            check((int) bag.invoke(1) == 8, "a bound array-element getter reads the element");
        });
        step("bindTo refuses a second bind on a bound getter", () -> {
            // The consequence of the narrowing above, and the reason it is a
            // parity fix rather than cosmetics: while `type()` still carried the
            // receiver, this second bind was ACCEPTED and silently OVERWROTE the
            // first capture. HotSpot refuses it -- the narrowed type has arity 0.
            MethodHandle bg = MethodHandles.lookup()
                    .findGetter(Holder.class, "i", int.class)
                    .bindTo(new Holder(11));
            boolean threw = false;
            try {
                MethodHandle again = bg.bindTo(new Holder(12));
                check(false, "the second bindTo was ACCEPTED and produced " + again.type());
            } catch (IllegalArgumentException expected) {
                threw = true;
            }
            check(threw, "a second bindTo on a bound getter must raise IllegalArgumentException");
        });
    }

    static int plus(int a, int b) {
        return a + b;
    }

    static int minus(int a, int b) {
        return a - b;
    }

    static int twice(int x) {
        return x * 2;
    }

    static long twiceLong(long x) {
        return x * 2L;
    }

    static boolean isPositive(int x) {
        return x >= 0;
    }

    static int negate(int x) {
        return -x;
    }

    static int boom(int x) {
        throw new IllegalStateException("boom " + x);
    }

    static int boomOther(int x) {
        throw new IllegalArgumentException("other " + x);
    }

    static int recover(IllegalStateException e, int x) {
        return 1000 + x;
    }

    static int sumAll(int[] xs) {
        return sumInts(xs);
    }

    static int sumInts(int[] xs) {
        int s = 0;
        for (int x : xs) {
            s += x;
        }
        return s;
    }

    static long sumLongs(long[] xs) {
        long s = 0L;
        for (long x : xs) {
            s += x;
        }
        return s;
    }

    static double sumDoubles(double[] xs) {
        double s = 0.0;
        for (double x : xs) {
            s += x;
        }
        return s;
    }

    static float sumFloats(float[] xs) {
        float s = 0.0f;
        for (float x : xs) {
            s += x;
        }
        return s;
    }

    static int sumBytes(byte[] xs) {
        int s = 0;
        for (byte x : xs) {
            s += x;
        }
        return s;
    }

    static int sumShorts(short[] xs) {
        int s = 0;
        for (short x : xs) {
            s += x;
        }
        return s;
    }

    static int sumChars(char[] xs) {
        int s = 0;
        for (char x : xs) {
            s += x;
        }
        return s;
    }

    static int countTrue(boolean[] xs) {
        int s = 0;
        for (boolean x : xs) {
            if (x) {
                s++;
            }
        }
        return s;
    }

    static String catStrings(String[] xs) {
        StringBuilder b = new StringBuilder();
        for (String x : xs) {
            b.append(x);
        }
        return b.toString();
    }

    static String catTwo(String a, String b) {
        return a + b;
    }

    static int countObjects(Object[] xs) {
        return xs.length;
    }

    /** The container the collector actually built -- {@code "[I"}, not a value. */
    static String classOfInts(int[] xs) {
        return xs.getClass().getName();
    }

    static String classOfLongs(long[] xs) {
        return xs.getClass().getName();
    }

    static String classOfStrings(String[] xs) {
        return xs.getClass().getName();
    }

    static String classOfFirst(Object[] xs) {
        return xs[0].getClass().getName();
    }

    static String labelled(String label, int[] xs) {
        return label + "=" + sumInts(xs);
    }

    static int labelledTimes(String s, int n) {
        return s.length() * n;
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

        // W6-8 / W4-1: the `unreflect*` family reaches exactly the members
        // `find*` reaches, so it has to ask the same mode question. Until
        // `3644142d5` it asked nothing at all, which made the two refusals
        // above reachable around in one line -- same Lookup, same member,
        // opposite answers. Nothing in the corpus asserted either polarity;
        // this is that vector.
        //
        // HotSpot 25 rule, from `MethodHandles.Lookup`: for `unreflect`, "If
        // the method's accessible flag is not set, access checking is performed
        // immediately on behalf of the lookup class", and the body is
        // `Lookup lookup = m.isAccessible() ? IMPL_LOOKUP : this;` -- so a set
        // flag is an unconditional ALLOW performed by the TRUSTED lookup, not a
        // discount. All three polarities are asserted because each one alone is
        // passable by a broken gate: a gate that asks nothing passes the second
        // and third, a gate that refuses every non-PRIVATE lookup passes the
        // first and second, and a gate that ignores the accessible flag passes
        // the first and second.
        //
        // ORDER IS LOAD-BEARING: `setAccessible(true)` is called only AFTER the
        // full-power positive, so the flag cannot be what makes that check
        // pass. That is the exact vacuity L15's field block was written to
        // avoid.
        Method secretM = Holder.class.getDeclaredMethod("secret", int.class);
        threw = false;
        try {
            pub.unreflect(secretM);
        } catch (IllegalAccessException expected) {
            threw = true;
        }
        check(threw, "publicLookup must not unreflect a private method");

        // BOTH positives INVOKE the handle and pin its result. `!= null` alone
        // is satisfied by a fabricated carrier with no invocable body -- the
        // fabricated-success species -- and the only other assertions in this
        // block are refusals, so a VM that returned a stand-in for every
        // `unreflect` passed the whole block. `Holder.secret(add)` is
        // `return i + add;` and `new Holder(n)` sets `i = n`, so the sum is a
        // value only that body can produce; a handle bound to the wrong member,
        // to the wrong receiver, or to nothing at all cannot answer it.
        MethodHandle fullPower = MethodHandles.lookup().unreflect(secretM);
        check(fullPower != null && (int) fullPower.invoke(new Holder(7), 35) == 42,
                "a full-power lookup must unreflect a private nestmate method into a"
                        + " handle that invokes to i+add");

        secretM.setAccessible(true);
        MethodHandle honoured = pub.unreflect(secretM);
        check(honoured != null && (int) honoured.invoke(new Holder(2), 40) == 42,
                "unreflect must honour a set accessible flag and yield a working handle");

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
        // The sections run as steps too, for the same reason the combinators
        // do: a VarHandle defect used to hide every access check behind it.
        step("lookupAndInvoke", RJdkHandles::lookupAndInvoke);
        adaptation();
        collectors();
        binding();
        step("accessChecks", RJdkHandles::accessChecks);
        step("varHandles", RJdkHandles::varHandles);
        step("varHandlesErased", RJdkHandles::varHandlesErased);
        System.out.println("CK RJdkHandles steps=" + steps);
        System.out.println("CK RJdkHandles checks=" + checks);
        if (!failures.isEmpty()) {
            // Named, all of them, in one run. That is the whole point of the
            // rewrite: the operator should not have to fix one combinator to
            // discover the next one is broken too.
            throw new AssertionError(failures.size() + " of " + steps
                    + " steps failed: " + failures);
        }
        System.out.println("PASS RJdkHandles (" + checks + " checks, " + steps + " steps)");
    }
}
