import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.invoke.WrongMethodTypeException;

/**
 * The {@code VarHandle} REFERENCE-read thin direct bind, exercised HOT.
 *
 * <p>{@code RJdkHandles} already covers {@code String bogus = (String)
 * intVarHandle.get(h)} — but it runs that line ONCE, in a cold method, so it
 * only ever tests the interpreter's route through the generic native funnel.
 * The bind this vector exists for is a compile-time decision: it replaces the
 * funnel with a baked {@code CALL} once the enclosing method is hot, and the
 * baked call carries an ERASED descriptor. Everything below therefore runs in a
 * loop long enough to be compiled at both the single-pass and the OSR door.
 *
 * <p>The three shapes are the three the recogniser classifies:
 *
 * <ul>
 *   <li>{@code REF_STRICT} — a declared return type no boxed primitive can
 *       satisfy. Bound, and the cold arm must still raise for a primitive
 *       variable read there.</li>
 *   <li>{@code REF_OBJECT} — {@code Ljava/lang/Object;}. Bound, and a box IS
 *       legal there, so reading an {@code int} variable at such a site must
 *       produce the {@code Integer} and NOT throw.</li>
 *   <li>a box-accepting type ({@code Number} and friends) — NOT bound, because
 *       whether a box satisfies it depends on which wrapper arrived.</li>
 * </ul>
 *
 * <p>Deterministic output only: HotSpot is the oracle and every line below has
 * to match it byte for byte.
 */
public class RJitVarHandleRefRead {

    static final int ROUNDS = Integer.getInteger("rjit.vh.rounds", 300_000);

    static class Holder {
        Node ref = new Node(7);
        Object any = "seed";
        int i = 11;
        Number num = Integer.valueOf(3);
    }

    static class Node {
        final int v;
        Node(int v) { this.v = v; }
    }

    static final VarHandle REF;   // Node ref
    static final VarHandle ANY;   // Object any
    static final VarHandle INT;   // int i
    static final VarHandle NUM;   // Number num

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            REF = l.findVarHandle(Holder.class, "ref", Node.class);
            ANY = l.findVarHandle(Holder.class, "any", Object.class);
            INT = l.findVarHandle(Holder.class, "i", int.class);
            NUM = l.findVarHandle(Holder.class, "num", Number.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int checks = 0;

    static void check(boolean ok, String what) {
        checks++;
        if (!ok) throw new AssertionError("FAIL " + what);
    }

    /** REF_STRICT, the happy path: a reference variable read at a `LNode;` site. */
    static long hotStrictRead(Holder h) {
        long acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            Node n = (Node) REF.get(h);
            Node a = (Node) REF.getAcquire(h);
            Node v = (Node) REF.getVolatile(h);
            Node o = (Node) REF.getOpaque(h);
            acc += n.v + a.v + v.v + o.v;
        }
        return acc;
    }

    /** REF_OBJECT: an `Ljava/lang/Object;` site over a reference variable. */
    static long hotObjectRead(Holder h) {
        long acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            Object o = ANY.get(h);
            acc += ((String) o).length();
        }
        return acc;
    }

    /**
     * REF_OBJECT over a PRIMITIVE variable: legal, and must BOX rather than
     * throw. This is the row that says the strict arm's raise is scoped to the
     * strict kind — an `Object` site accepts any wrapper.
     */
    static long hotObjectReadOfPrimitive(Holder h) {
        long acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            Object o = INT.get(h);
            acc += ((Integer) o).intValue();
        }
        return acc;
    }

    /** Not bound: `Number` is box-accepting, and reading a `Number` field there is legal. */
    static long hotNumberRead(Holder h) {
        long acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            Number n = (Number) NUM.get(h);
            acc += n.intValue();
        }
        return acc;
    }

    /**
     * REF_STRICT over a PRIMITIVE variable, run HOT.
     *
     * <p>This is the W6-1 fire set, and the one shape the bind's erased
     * stand-in descriptor could have silently disabled: the site declares
     * `Ljava/lang/String;`, the variable is an `int`, so the access produces an
     * `Integer` that no `String` can hold. The interpreter raises it from
     * `unbox_poly_return_checked`; compiled code has to raise it from the
     * helper's own cold arm.
     */
    static int hotStrictWrongType(Holder h) {
        int threw = 0;
        for (int i = 0; i < ROUNDS / 100; i++) {
            try {
                String bogus = (String) INT.get(h);
                if (bogus == null) threw += 1000;   // unreachable, and not silent if it is not
            } catch (WrongMethodTypeException | ClassCastException expected) {
                threw++;
            }
        }
        return threw;
    }

    public static void main(String[] args) {
        Holder h = new Holder();

        long strict = hotStrictRead(h);
        check(strict == 4L * ROUNDS * 7, "REF_STRICT reference read");

        long obj = hotObjectRead(h);
        check(obj == 4L * ROUNDS, "REF_OBJECT reference read");

        long boxed = hotObjectReadOfPrimitive(h);
        check(boxed == 11L * ROUNDS, "REF_OBJECT read of a primitive variable boxes");

        long num = hotNumberRead(h);
        check(num == 3L * ROUNDS, "a box-accepting site is not bound and still reads");

        int threw = hotStrictWrongType(h);
        check(threw == ROUNDS / 100, "every hot REF_STRICT wrong-type read threw");

        // The reads must still see writes made through the same handle, i.e.
        // the bind did not cache a value across a store.
        REF.set(h, new Node(41));
        check(((Node) REF.get(h)).v == 41, "a bound read sees a later store");
        ANY.setVolatile(h, "abcd");
        check("abcd".equals(ANY.get(h)), "a bound Object read sees a later store");

        // A NULL COORDINATE is deliberately NOT asserted here.
        //
        // HotSpot raises NullPointerException for `REF.get((Holder) null)`;
        // CratonVM answers `null` for a reference read, `0` for a primitive
        // read, and silently does nothing for a `set`. That is a pre-existing
        // divergence in the generic natives, not a property of this bind --
        // it reproduces identically with
        // `CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT=0`, i.e. with no bind at all
        // and every access on the funnel. Asserting it here would make this
        // vector red for a reason it does not test.
        //
        // Filed as
        // fixed-bugs/varhandle-null-coordinate-answers-instead-of-throwing-FIXED-20260902.md.

        // The count line carries the COUNT and nothing else: harness guard G6
        // reads it with `harness_check_count`, and a second `key=value` on it
        // makes that return a non-numeric string, which silently no-ops G3 --
        // the guard that notices a vector whose comparison the suite cannot
        // see. Every other value gets its own line.
        System.out.println("CK RJitVarHandleRefRead strict=" + strict);
        System.out.println("CK RJitVarHandleRefRead obj=" + obj);
        System.out.println("CK RJitVarHandleRefRead boxed=" + boxed);
        System.out.println("CK RJitVarHandleRefRead num=" + num);
        System.out.println("CK RJitVarHandleRefRead threw=" + threw);
        System.out.println("CK RJitVarHandleRefRead checks=" + checks);
        System.out.println("PASS RJitVarHandleRefRead");
    }
}
