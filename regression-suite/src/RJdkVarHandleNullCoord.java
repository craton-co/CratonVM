import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/**
 * Every {@code VarHandle} access mode, reached with a NULL coordinate.
 *
 * <p>HotSpot raises {@code NullPointerException} for all of them. CratonVM used
 * to answer instead — {@code null} for a reference read, {@code 0} for a
 * primitive read, and a silent no-op for a write, which is a lost store with no
 * signal anywhere. See
 * varhandle-null-coordinate-answers-instead-of-throwing-FIXED-20260902.md (internal).
 *
 * <p>The matrix is generated rather than hand-typed: each row is a
 * signature-polymorphic call site whose exact static types decide which access
 * mode is invoked, so a stray cast silently tests a different mode. The last
 * two rows are the control — a STATIC-field handle takes no coordinate, so
 * there is nothing to be null and both VMs must answer normally.
 */
public class RJdkVarHandleNullCoord {

    static final Object VOID = "void";

    static class H {
        int i = 11;
        Object o = "seed";
    }

    static int stat = 3;

    static final VarHandle VH_I;   // H.i, an int instance field
    static final VarHandle VH_O;   // H.o, an Object instance field
    static final VarHandle VH_A;   // int[] element
    static final VarHandle VH_S;   // static int

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            VH_I = l.findVarHandle(H.class, "i", int.class);
            VH_O = l.findVarHandle(H.class, "o", Object.class);
            VH_A = MethodHandles.arrayElementVarHandle(int[].class);
            VH_S = l.findStaticVarHandle(RJdkVarHandleNullCoord.class, "stat", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Body { Object run() throws Throwable; }

    static int checks = 0;

    /**
     * One row. Prints the OUTCOME — the throwable's class name, or the value
     * that came back instead — and nothing else, so the cross-VM diff is
     * exactly "did the two VMs do the same thing".
     */
    static void probe(String label, Body b) {
        String outcome;
        try {
            Object v = b.run();
            outcome = (v == VOID) ? "returned-normally" : "returned:" + v;
        } catch (Throwable t) {
            outcome = "threw:" + t.getClass().getName();
        }
        checks++;
        System.out.println("CK RJdkVarHandleNullCoord " + label + "=" + outcome);
    }

    public static void main(String[] args) {
        probe("int-instance.get", () -> { return (int) VH_I.get((H) null); });
        probe("int-instance.getVolatile", () -> { return (int) VH_I.getVolatile((H) null); });
        probe("int-instance.getOpaque", () -> { return (int) VH_I.getOpaque((H) null); });
        probe("int-instance.getAcquire", () -> { return (int) VH_I.getAcquire((H) null); });
        probe("int-instance.set", () -> { VH_I.set((H) null, 7); return VOID; });
        probe("int-instance.setVolatile", () -> { VH_I.setVolatile((H) null, 7); return VOID; });
        probe("int-instance.setOpaque", () -> { VH_I.setOpaque((H) null, 7); return VOID; });
        probe("int-instance.setRelease", () -> { VH_I.setRelease((H) null, 7); return VOID; });
        probe("int-instance.compareAndSet", () -> { return VH_I.compareAndSet((H) null, 0, 7); });
        probe("int-instance.weakCompareAndSet", () -> { return VH_I.weakCompareAndSet((H) null, 0, 7); });
        probe("int-instance.weakCompareAndSetPlain", () -> { return VH_I.weakCompareAndSetPlain((H) null, 0, 7); });
        probe("int-instance.weakCompareAndSetAcquire", () -> { return VH_I.weakCompareAndSetAcquire((H) null, 0, 7); });
        probe("int-instance.weakCompareAndSetRelease", () -> { return VH_I.weakCompareAndSetRelease((H) null, 0, 7); });
        probe("int-instance.compareAndExchange", () -> { return (int) VH_I.compareAndExchange((H) null, 0, 7); });
        probe("int-instance.compareAndExchangeAcquire", () -> { return (int) VH_I.compareAndExchangeAcquire((H) null, 0, 7); });
        probe("int-instance.compareAndExchangeRelease", () -> { return (int) VH_I.compareAndExchangeRelease((H) null, 0, 7); });
        probe("int-instance.getAndSet", () -> { return (int) VH_I.getAndSet((H) null, 7); });
        probe("int-instance.getAndSetAcquire", () -> { return (int) VH_I.getAndSetAcquire((H) null, 7); });
        probe("int-instance.getAndSetRelease", () -> { return (int) VH_I.getAndSetRelease((H) null, 7); });
        probe("int-instance.getAndAdd", () -> { return (int) VH_I.getAndAdd((H) null, 7); });
        probe("int-instance.getAndAddAcquire", () -> { return (int) VH_I.getAndAddAcquire((H) null, 7); });
        probe("int-instance.getAndAddRelease", () -> { return (int) VH_I.getAndAddRelease((H) null, 7); });
        probe("int-instance.getAndBitwiseOr", () -> { return (int) VH_I.getAndBitwiseOr((H) null, 7); });
        probe("int-instance.getAndBitwiseOrAcquire", () -> { return (int) VH_I.getAndBitwiseOrAcquire((H) null, 7); });
        probe("int-instance.getAndBitwiseOrRelease", () -> { return (int) VH_I.getAndBitwiseOrRelease((H) null, 7); });
        probe("int-instance.getAndBitwiseAnd", () -> { return (int) VH_I.getAndBitwiseAnd((H) null, 7); });
        probe("int-instance.getAndBitwiseAndAcquire", () -> { return (int) VH_I.getAndBitwiseAndAcquire((H) null, 7); });
        probe("int-instance.getAndBitwiseAndRelease", () -> { return (int) VH_I.getAndBitwiseAndRelease((H) null, 7); });
        probe("int-instance.getAndBitwiseXor", () -> { return (int) VH_I.getAndBitwiseXor((H) null, 7); });
        probe("int-instance.getAndBitwiseXorAcquire", () -> { return (int) VH_I.getAndBitwiseXorAcquire((H) null, 7); });
        probe("int-instance.getAndBitwiseXorRelease", () -> { return (int) VH_I.getAndBitwiseXorRelease((H) null, 7); });
        probe("ref-instance.get", () -> { return VH_O.get((H) null); });
        probe("ref-instance.getVolatile", () -> { return VH_O.getVolatile((H) null); });
        probe("ref-instance.getOpaque", () -> { return VH_O.getOpaque((H) null); });
        probe("ref-instance.getAcquire", () -> { return VH_O.getAcquire((H) null); });
        probe("ref-instance.set", () -> { VH_O.set((H) null, "x"); return VOID; });
        probe("ref-instance.setVolatile", () -> { VH_O.setVolatile((H) null, "x"); return VOID; });
        probe("ref-instance.setOpaque", () -> { VH_O.setOpaque((H) null, "x"); return VOID; });
        probe("ref-instance.setRelease", () -> { VH_O.setRelease((H) null, "x"); return VOID; });
        probe("ref-instance.compareAndSet", () -> { return VH_O.compareAndSet((H) null, null, "x"); });
        probe("ref-instance.weakCompareAndSet", () -> { return VH_O.weakCompareAndSet((H) null, null, "x"); });
        probe("ref-instance.weakCompareAndSetPlain", () -> { return VH_O.weakCompareAndSetPlain((H) null, null, "x"); });
        probe("ref-instance.weakCompareAndSetAcquire", () -> { return VH_O.weakCompareAndSetAcquire((H) null, null, "x"); });
        probe("ref-instance.weakCompareAndSetRelease", () -> { return VH_O.weakCompareAndSetRelease((H) null, null, "x"); });
        probe("ref-instance.compareAndExchange", () -> { return VH_O.compareAndExchange((H) null, null, "x"); });
        probe("ref-instance.compareAndExchangeAcquire", () -> { return VH_O.compareAndExchangeAcquire((H) null, null, "x"); });
        probe("ref-instance.compareAndExchangeRelease", () -> { return VH_O.compareAndExchangeRelease((H) null, null, "x"); });
        probe("ref-instance.getAndSet", () -> { return VH_O.getAndSet((H) null, "x"); });
        probe("ref-instance.getAndSetAcquire", () -> { return VH_O.getAndSetAcquire((H) null, "x"); });
        probe("ref-instance.getAndSetRelease", () -> { return VH_O.getAndSetRelease((H) null, "x"); });
        probe("int-array.get", () -> { return (int) VH_A.get((int[]) null, 0); });
        probe("int-array.getVolatile", () -> { return (int) VH_A.getVolatile((int[]) null, 0); });
        probe("int-array.getOpaque", () -> { return (int) VH_A.getOpaque((int[]) null, 0); });
        probe("int-array.getAcquire", () -> { return (int) VH_A.getAcquire((int[]) null, 0); });
        probe("int-array.set", () -> { VH_A.set((int[]) null, 0, 7); return VOID; });
        probe("int-array.setVolatile", () -> { VH_A.setVolatile((int[]) null, 0, 7); return VOID; });
        probe("int-array.setOpaque", () -> { VH_A.setOpaque((int[]) null, 0, 7); return VOID; });
        probe("int-array.setRelease", () -> { VH_A.setRelease((int[]) null, 0, 7); return VOID; });
        probe("int-array.compareAndSet", () -> { return VH_A.compareAndSet((int[]) null, 0, 0, 7); });
        probe("int-array.weakCompareAndSet", () -> { return VH_A.weakCompareAndSet((int[]) null, 0, 0, 7); });
        probe("int-array.weakCompareAndSetPlain", () -> { return VH_A.weakCompareAndSetPlain((int[]) null, 0, 0, 7); });
        probe("int-array.weakCompareAndSetAcquire", () -> { return VH_A.weakCompareAndSetAcquire((int[]) null, 0, 0, 7); });
        probe("int-array.weakCompareAndSetRelease", () -> { return VH_A.weakCompareAndSetRelease((int[]) null, 0, 0, 7); });
        probe("int-array.compareAndExchange", () -> { return (int) VH_A.compareAndExchange((int[]) null, 0, 0, 7); });
        probe("int-array.compareAndExchangeAcquire", () -> { return (int) VH_A.compareAndExchangeAcquire((int[]) null, 0, 0, 7); });
        probe("int-array.compareAndExchangeRelease", () -> { return (int) VH_A.compareAndExchangeRelease((int[]) null, 0, 0, 7); });
        probe("int-array.getAndSet", () -> { return (int) VH_A.getAndSet((int[]) null, 0, 7); });
        probe("int-array.getAndSetAcquire", () -> { return (int) VH_A.getAndSetAcquire((int[]) null, 0, 7); });
        probe("int-array.getAndSetRelease", () -> { return (int) VH_A.getAndSetRelease((int[]) null, 0, 7); });
        probe("int-array.getAndAdd", () -> { return (int) VH_A.getAndAdd((int[]) null, 0, 7); });
        probe("int-array.getAndAddAcquire", () -> { return (int) VH_A.getAndAddAcquire((int[]) null, 0, 7); });
        probe("int-array.getAndAddRelease", () -> { return (int) VH_A.getAndAddRelease((int[]) null, 0, 7); });
        probe("int-static.get", () -> { return (int) VH_S.get(); });
        probe("int-static.set", () -> { VH_S.set(7); return VOID; });

        System.out.println("CK RJdkVarHandleNullCoord checks=" + checks);
        System.out.println("PASS RJdkVarHandleNullCoord");
    }
}
