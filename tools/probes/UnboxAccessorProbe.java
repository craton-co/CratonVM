import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

// Behavioural check on the optimizing tier's inline field accessors --
// `Long.longValue`, `Integer.intValue`, `AtomicLong.get`,
// `AtomicInteger.incrementAndGet` / `decrementAndGet`, `AtomicLong.getAndAdd`.
//
// The four Atomic families used to make the IR tier refuse the WHOLE METHOD
// (`[c2-supersede] call-site intrinsics: refused_method`, 21 H2 methods between
// them). They are now `Op::FieldAccessor` nodes: a null check, an exact
// landed first: a null check, an exact receiver class-id guard, a per-object
// COMPACT/LEGACY branch, and one `MOV` or `LOCK XADD`.
//
// The unit tests in `jit/src/ir_lower.rs` assert the EMITTED BYTES against a
// synthetic receiver. This asserts what the compiled body does against real
// heap objects and real class ids -- and, because every expected value here is
// specified by the JLS and by the `java.util.concurrent.atomic` javadoc rather
// than by CratonVM, it is its own HotSpot oracle: run it on HotSpot and it must
// print the same line.
//
//   java -cp probes UnboxAccessorProbe            # oracle
//   cratonvm -cp probes UnboxAccessorProbe        # under test
//
// # Why every kernel is called in a warm loop
//
// A cold call runs in the interpreter, which reaches the registered native and
// proves nothing about any compiled body. Each kernel below is called well past
// the compile threshold BEFORE its result is asserted, so the value under test
// is the one the compiled body produced. A probe that asserts on cold calls is
// the vacuous green this family's own site counters were added to expose.
public class UnboxAccessorProbe {
    static final int WARM = 60000;
    static int fails = 0;

    static void check(String what, long got, long want) {
        if (got != want) {
            System.out.println("FAIL " + what + ": got " + got + " want " + want);
            fails++;
        }
    }

    // ---- The six kernels. Each is a whole method, so each is a compile unit
    // ---- whose ONLY interesting site is the accessor under test.

    static long unboxLong(Long v) {
        return v.longValue();
    }

    static int unboxInt(Integer v) {
        return v.intValue();
    }

    static long atomicGet(AtomicLong a) {
        return a.get();
    }

    static int incGet(AtomicInteger a) {
        return a.incrementAndGet();
    }

    static int decGet(AtomicInteger a) {
        return a.decrementAndGet();
    }

    static long getAndAdd(AtomicLong a, long d) {
        return a.getAndAdd(d);
    }

    // The accessor inside a body with OTHER work in it. The whole point of
    // lowering these as nodes rather than lifting the refusal is that the
    // surrounding method keeps the optimizing tier; a kernel that is nothing
    // but the accessor would not notice if it did not.
    static long mixed(AtomicLong a, Long boxed, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += a.get() + boxed.longValue() + Math.max(i, 3);
        }
        return acc;
    }

    // A receiver the class-id guard must REJECT.
    //
    // `AtomicLong` is not final, so this is a legal receiver of `atomicGet`;
    // its `ObjectHeader` class id is its own, so the compiled body's exact
    // guard misses and the site DEOPTS. `get()` and `getAndAdd()` are `final`
    // in the JDK, so the value the interpreter then produces is the same one
    // the inline path would have -- which is exactly why this case tests the
    // guard's RECOVERY rather than an override's result: what must hold is
    // that the answer is still right and that the site keeps working for the
    // base class afterwards.
    //
    // (An override is not expressible here, and that is worth writing down: the
    // two methods being final means the guard can never LEGITIMATELY miss for
    // them, the same position `box_unbox_intrinsic_shape` records for
    // `java/lang/Long`. The guard is emitted anyway because its miss is a
    // deopt, not a wrong answer.)
    static class SubLong extends AtomicLong {
        SubLong(long v) { super(v); }
    }

    public static void main(String[] args) {
        // ---- The three loads -------------------------------------------
        Long boxedL = Long.valueOf(0x1234_5678_9ABC_DEF0L);
        long sum = 0;
        for (int i = 0; i < WARM; i++) sum += unboxLong(boxedL);
        check("Long.longValue", sum, 0x1234_5678_9ABC_DEF0L * WARM);
        // The edges: a value whose LOW half is zero separates a 64-bit load
        // from a 32-bit one, and MIN_VALUE is the bit pattern that collides
        // with the wide-helper deopt sentinel on the paths that use one.
        check("Long.longValue(MIN)", unboxLong(Long.valueOf(Long.MIN_VALUE)), Long.MIN_VALUE);
        check("Long.longValue(-1)", unboxLong(Long.valueOf(-1L)), -1L);
        check("Long.longValue(1<<40)", unboxLong(Long.valueOf(1L << 40)), 1L << 40);

        // `Integer.intValue` must SIGN-extend. Values outside the -128..127
        // `Integer` cache are used deliberately: a cached box and a fresh one
        // are different objects, and only the fresh ones prove the load rather
        // than a constant-folded cache hit.
        Integer boxedI = Integer.valueOf(-70000);
        long isum = 0;
        for (int i = 0; i < WARM; i++) isum += unboxInt(boxedI);
        check("Integer.intValue", isum, -70000L * WARM);
        check("Integer.intValue(MIN)", unboxInt(Integer.valueOf(Integer.MIN_VALUE)), Integer.MIN_VALUE);
        check("Integer.intValue(MAX)", unboxInt(Integer.valueOf(Integer.MAX_VALUE)), Integer.MAX_VALUE);
        check("Integer.intValue(-1)", unboxInt(Integer.valueOf(-1)), -1);

        AtomicLong al = new AtomicLong(0x0BAD_CAFE_0000_0001L);
        long gsum = 0;
        for (int i = 0; i < WARM; i++) gsum += atomicGet(al);
        check("AtomicLong.get", gsum, 0x0BAD_CAFE_0000_0001L * WARM);

        // ---- The three LOCK XADD forms ---------------------------------
        //
        // Asserted on the RETURNED value AND on the field afterwards. Only the
        // pair separates `incrementAndGet` (post-add) from `getAndIncrement`
        // (pre-add): each leaves the same field value and returns a different
        // number.
        AtomicInteger ai = new AtomicInteger(0);
        int last = 0;
        for (int i = 0; i < WARM; i++) last = incGet(ai);
        check("AtomicInteger.incrementAndGet returns post-add", last, WARM);
        check("AtomicInteger.incrementAndGet field", ai.get(), WARM);

        for (int i = 0; i < WARM; i++) last = decGet(ai);
        check("AtomicInteger.decrementAndGet returns post-add", last, 0);
        check("AtomicInteger.decrementAndGet field", ai.get(), 0);

        // Across zero and across the 32-bit boundary, which a `MOVSXD` emitted
        // where a zero-extend belongs (or the reverse) gets wrong.
        ai.set(1);
        check("decrementAndGet to zero", decGet(ai), 0);
        check("decrementAndGet below zero", decGet(ai), -1);
        ai.set(Integer.MAX_VALUE);
        check("incrementAndGet wraps like Java", incGet(ai), Integer.MIN_VALUE);
        ai.set(Integer.MIN_VALUE);
        check("decrementAndGet wraps like Java", decGet(ai), Integer.MAX_VALUE);

        AtomicLong ga = new AtomicLong(0);
        long lastL = 0;
        for (int i = 0; i < WARM; i++) lastL = getAndAdd(ga, 3L);
        check("AtomicLong.getAndAdd returns PRE-add", lastL, 3L * (WARM - 1));
        check("AtomicLong.getAndAdd field", ga.get(), 3L * WARM);
        // A negative delta and a delta that does not fit 32 bits: the runtime
        // delta is a full 64-bit operand, and truncating it is the mistake a
        // 32-bit `MOV` into the delta register would make.
        ga.set(0);
        check("getAndAdd(-5)", getAndAdd(ga, -5L), 0L);
        check("getAndAdd(-5) field", ga.get(), -5L);
        ga.set(0);
        check("getAndAdd(1<<40)", getAndAdd(ga, 1L << 40), 0L);
        check("getAndAdd(1<<40) field", ga.get(), 1L << 40);

        // ---- The accessor inside a real body ---------------------------
        AtomicLong ma = new AtomicLong(7);
        Long mb = Long.valueOf(11);
        long mixWant = 0;
        for (int i = 0; i < 1000; i++) mixWant += 7 + 11 + Math.max(i, 3);
        long mixGot = 0;
        for (int r = 0; r < 200; r++) mixGot = mixed(ma, mb, 1000);
        check("mixed body", mixGot, mixWant);

        // ---- The receiver guard ----------------------------------------
        //
        // Warmed on the base class FIRST, so `atomicGet` / `getAndAdd` are
        // compiled with the guard in place before the subclass is ever passed.
        // The subclass then MISSES the exact class-id guard and takes the deopt
        // edge; the answer must still be right, and the base class must still
        // work afterwards -- which is what says the deopt recovered rather than
        // poisoned the site.
        SubLong sub = new SubLong(999L);
        check("subclass receiver still reads its own field", atomicGet(sub), 999L);
        check("subclass getAndAdd returns PRE-add", getAndAdd(sub, 5L), 999L);
        check("subclass getAndAdd updated the field", atomicGet(sub), 1004L);
        // Repeatedly, so a site that deopts on every call still answers
        // correctly on every call rather than only on the first.
        long subSum = 0;
        for (int i = 0; i < 5000; i++) subSum += atomicGet(sub);
        check("subclass receiver over a hot loop", subSum, 1004L * 5000);
        al.set(42);
        check("base class still works after the guard missed", atomicGet(al), 42L);
        for (int i = 0; i < WARM; i++) gsum = atomicGet(al);
        check("and still works once recompiled", gsum, 42L);

        // ---- Null receivers --------------------------------------------
        //
        // The inline path emits no exception of its own: a null receiver
        // DEOPTS, the interpreter re-executes the invoke, and the real
        // implementation raises the NPE. That is only observable from Java as
        // "the right exception still arrives", which is what this checks -- on
        // a HOT method, or it tests the interpreter.
        expectNpe("Long.longValue(null)", () -> unboxLong(null));
        expectNpe("Integer.intValue(null)", () -> unboxInt(null));
        expectNpe("AtomicLong.get(null)", () -> atomicGet(null));
        expectNpe("AtomicInteger.incrementAndGet(null)", () -> incGet(null));
        expectNpe("AtomicInteger.decrementAndGet(null)", () -> decGet(null));
        expectNpe("AtomicLong.getAndAdd(null)", () -> getAndAdd(null, 1L));

        System.out.println(fails == 0
            ? "UNBOX ACCESSOR PROBE OK"
            : "UNBOX ACCESSOR PROBE FAILED " + fails);
    }

    static void expectNpe(String what, Runnable r) {
        try {
            r.run();
            System.out.println("FAIL " + what + ": no NullPointerException");
            fails++;
        } catch (NullPointerException e) {
            // as specified
        }
    }
}
