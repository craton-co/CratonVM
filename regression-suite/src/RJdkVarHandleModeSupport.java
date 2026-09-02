import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/**
 * Which access modes a {@code VarHandle} supports, by VARIABLE TYPE.
 *
 * <p>The rule is not uniform, which is the whole reason this is a generated
 * sweep: {@code getAndBitwise*} is supported for {@code boolean} while
 * {@code getAndAdd*} is not, and neither is supported for a reference. A
 * hand-picked handful of rows cannot tell a rule from a coincidence.
 *
 * <p>The ORDER question is swept alongside it, because an implementation has to
 * pick one and HotSpot's choice is the oracle: an unsupported mode beats a null
 * coordinate, uniformly, for all ten variable types.
 *
 * <p>Each row prints only the outcome — the throwable's class, or the value
 * that came back — so the cross-VM comparison is a plain diff.
 */
public class RJdkVarHandleModeSupport {

    static final Object VOID = "void";

    static class H {
        boolean z;
        byte b;
        char c;
        short s;
        int i;
        long j;
        float f;
        double d;
        Object o;
        String t;
        final int fin = 9;
    }

    static final H h = new H();
    static final H h2 = new H();

    static final VarHandle VH_Z;
    static final VarHandle VH_B;
    static final VarHandle VH_C;
    static final VarHandle VH_S;
    static final VarHandle VH_I;
    static final VarHandle VH_J;
    static final VarHandle VH_F;
    static final VarHandle VH_D;
    static final VarHandle VH_O;
    static final VarHandle VH_T;
    static final VarHandle VH_FIN;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            VH_Z = l.findVarHandle(H.class, "z", boolean.class);
            VH_B = l.findVarHandle(H.class, "b", byte.class);
            VH_C = l.findVarHandle(H.class, "c", char.class);
            VH_S = l.findVarHandle(H.class, "s", short.class);
            VH_I = l.findVarHandle(H.class, "i", int.class);
            VH_J = l.findVarHandle(H.class, "j", long.class);
            VH_F = l.findVarHandle(H.class, "f", float.class);
            VH_D = l.findVarHandle(H.class, "d", double.class);
            VH_O = l.findVarHandle(H.class, "o", Object.class);
            VH_T = l.findVarHandle(H.class, "t", String.class);
            VH_FIN = l.findVarHandle(H.class, "fin", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Body { Object run() throws Throwable; }

    static int checks = 0;

    static void probe(String label, Body b) {
        String outcome;
        try {
            Object v = b.run();
            outcome = (v == VOID) ? "returned-normally" : "returned:" + v;
        } catch (Throwable t) {
            outcome = "threw:" + t.getClass().getName();
        }
        checks++;
        System.out.println("CK RJdkVarHandleModeSupport " + label + "=" + outcome);
    }

    public static void main(String[] args) {
        probe("boolean.getAndAdd", () -> { return (boolean) VH_Z.getAndAdd(h, true); });
        probe("boolean.getAndAddAcquire", () -> { return (boolean) VH_Z.getAndAddAcquire(h, true); });
        probe("boolean.getAndAddRelease", () -> { return (boolean) VH_Z.getAndAddRelease(h, true); });
        probe("boolean.getAndBitwiseOr", () -> { return (boolean) VH_Z.getAndBitwiseOr(h, true); });
        probe("boolean.getAndBitwiseOrAcquire", () -> { return (boolean) VH_Z.getAndBitwiseOrAcquire(h, true); });
        probe("boolean.getAndBitwiseOrRelease", () -> { return (boolean) VH_Z.getAndBitwiseOrRelease(h, true); });
        probe("boolean.getAndBitwiseAnd", () -> { return (boolean) VH_Z.getAndBitwiseAnd(h, true); });
        probe("boolean.getAndBitwiseAndAcquire", () -> { return (boolean) VH_Z.getAndBitwiseAndAcquire(h, true); });
        probe("boolean.getAndBitwiseAndRelease", () -> { return (boolean) VH_Z.getAndBitwiseAndRelease(h, true); });
        probe("boolean.getAndBitwiseXor", () -> { return (boolean) VH_Z.getAndBitwiseXor(h, true); });
        probe("boolean.getAndBitwiseXorAcquire", () -> { return (boolean) VH_Z.getAndBitwiseXorAcquire(h, true); });
        probe("boolean.getAndBitwiseXorRelease", () -> { return (boolean) VH_Z.getAndBitwiseXorRelease(h, true); });
        probe("byte.getAndAdd", () -> { return (byte) VH_B.getAndAdd(h, (byte) 1); });
        probe("byte.getAndAddAcquire", () -> { return (byte) VH_B.getAndAddAcquire(h, (byte) 1); });
        probe("byte.getAndAddRelease", () -> { return (byte) VH_B.getAndAddRelease(h, (byte) 1); });
        probe("byte.getAndBitwiseOr", () -> { return (byte) VH_B.getAndBitwiseOr(h, (byte) 1); });
        probe("byte.getAndBitwiseOrAcquire", () -> { return (byte) VH_B.getAndBitwiseOrAcquire(h, (byte) 1); });
        probe("byte.getAndBitwiseOrRelease", () -> { return (byte) VH_B.getAndBitwiseOrRelease(h, (byte) 1); });
        probe("byte.getAndBitwiseAnd", () -> { return (byte) VH_B.getAndBitwiseAnd(h, (byte) 1); });
        probe("byte.getAndBitwiseAndAcquire", () -> { return (byte) VH_B.getAndBitwiseAndAcquire(h, (byte) 1); });
        probe("byte.getAndBitwiseAndRelease", () -> { return (byte) VH_B.getAndBitwiseAndRelease(h, (byte) 1); });
        probe("byte.getAndBitwiseXor", () -> { return (byte) VH_B.getAndBitwiseXor(h, (byte) 1); });
        probe("byte.getAndBitwiseXorAcquire", () -> { return (byte) VH_B.getAndBitwiseXorAcquire(h, (byte) 1); });
        probe("byte.getAndBitwiseXorRelease", () -> { return (byte) VH_B.getAndBitwiseXorRelease(h, (byte) 1); });
        probe("char.getAndAdd", () -> { return (char) VH_C.getAndAdd(h, (char) 1); });
        probe("char.getAndAddAcquire", () -> { return (char) VH_C.getAndAddAcquire(h, (char) 1); });
        probe("char.getAndAddRelease", () -> { return (char) VH_C.getAndAddRelease(h, (char) 1); });
        probe("char.getAndBitwiseOr", () -> { return (char) VH_C.getAndBitwiseOr(h, (char) 1); });
        probe("char.getAndBitwiseOrAcquire", () -> { return (char) VH_C.getAndBitwiseOrAcquire(h, (char) 1); });
        probe("char.getAndBitwiseOrRelease", () -> { return (char) VH_C.getAndBitwiseOrRelease(h, (char) 1); });
        probe("char.getAndBitwiseAnd", () -> { return (char) VH_C.getAndBitwiseAnd(h, (char) 1); });
        probe("char.getAndBitwiseAndAcquire", () -> { return (char) VH_C.getAndBitwiseAndAcquire(h, (char) 1); });
        probe("char.getAndBitwiseAndRelease", () -> { return (char) VH_C.getAndBitwiseAndRelease(h, (char) 1); });
        probe("char.getAndBitwiseXor", () -> { return (char) VH_C.getAndBitwiseXor(h, (char) 1); });
        probe("char.getAndBitwiseXorAcquire", () -> { return (char) VH_C.getAndBitwiseXorAcquire(h, (char) 1); });
        probe("char.getAndBitwiseXorRelease", () -> { return (char) VH_C.getAndBitwiseXorRelease(h, (char) 1); });
        probe("short.getAndAdd", () -> { return (short) VH_S.getAndAdd(h, (short) 1); });
        probe("short.getAndAddAcquire", () -> { return (short) VH_S.getAndAddAcquire(h, (short) 1); });
        probe("short.getAndAddRelease", () -> { return (short) VH_S.getAndAddRelease(h, (short) 1); });
        probe("short.getAndBitwiseOr", () -> { return (short) VH_S.getAndBitwiseOr(h, (short) 1); });
        probe("short.getAndBitwiseOrAcquire", () -> { return (short) VH_S.getAndBitwiseOrAcquire(h, (short) 1); });
        probe("short.getAndBitwiseOrRelease", () -> { return (short) VH_S.getAndBitwiseOrRelease(h, (short) 1); });
        probe("short.getAndBitwiseAnd", () -> { return (short) VH_S.getAndBitwiseAnd(h, (short) 1); });
        probe("short.getAndBitwiseAndAcquire", () -> { return (short) VH_S.getAndBitwiseAndAcquire(h, (short) 1); });
        probe("short.getAndBitwiseAndRelease", () -> { return (short) VH_S.getAndBitwiseAndRelease(h, (short) 1); });
        probe("short.getAndBitwiseXor", () -> { return (short) VH_S.getAndBitwiseXor(h, (short) 1); });
        probe("short.getAndBitwiseXorAcquire", () -> { return (short) VH_S.getAndBitwiseXorAcquire(h, (short) 1); });
        probe("short.getAndBitwiseXorRelease", () -> { return (short) VH_S.getAndBitwiseXorRelease(h, (short) 1); });
        probe("int.getAndAdd", () -> { return (int) VH_I.getAndAdd(h, 1); });
        probe("int.getAndAddAcquire", () -> { return (int) VH_I.getAndAddAcquire(h, 1); });
        probe("int.getAndAddRelease", () -> { return (int) VH_I.getAndAddRelease(h, 1); });
        probe("int.getAndBitwiseOr", () -> { return (int) VH_I.getAndBitwiseOr(h, 1); });
        probe("int.getAndBitwiseOrAcquire", () -> { return (int) VH_I.getAndBitwiseOrAcquire(h, 1); });
        probe("int.getAndBitwiseOrRelease", () -> { return (int) VH_I.getAndBitwiseOrRelease(h, 1); });
        probe("int.getAndBitwiseAnd", () -> { return (int) VH_I.getAndBitwiseAnd(h, 1); });
        probe("int.getAndBitwiseAndAcquire", () -> { return (int) VH_I.getAndBitwiseAndAcquire(h, 1); });
        probe("int.getAndBitwiseAndRelease", () -> { return (int) VH_I.getAndBitwiseAndRelease(h, 1); });
        probe("int.getAndBitwiseXor", () -> { return (int) VH_I.getAndBitwiseXor(h, 1); });
        probe("int.getAndBitwiseXorAcquire", () -> { return (int) VH_I.getAndBitwiseXorAcquire(h, 1); });
        probe("int.getAndBitwiseXorRelease", () -> { return (int) VH_I.getAndBitwiseXorRelease(h, 1); });
        probe("long.getAndAdd", () -> { return (long) VH_J.getAndAdd(h, 1L); });
        probe("long.getAndAddAcquire", () -> { return (long) VH_J.getAndAddAcquire(h, 1L); });
        probe("long.getAndAddRelease", () -> { return (long) VH_J.getAndAddRelease(h, 1L); });
        probe("long.getAndBitwiseOr", () -> { return (long) VH_J.getAndBitwiseOr(h, 1L); });
        probe("long.getAndBitwiseOrAcquire", () -> { return (long) VH_J.getAndBitwiseOrAcquire(h, 1L); });
        probe("long.getAndBitwiseOrRelease", () -> { return (long) VH_J.getAndBitwiseOrRelease(h, 1L); });
        probe("long.getAndBitwiseAnd", () -> { return (long) VH_J.getAndBitwiseAnd(h, 1L); });
        probe("long.getAndBitwiseAndAcquire", () -> { return (long) VH_J.getAndBitwiseAndAcquire(h, 1L); });
        probe("long.getAndBitwiseAndRelease", () -> { return (long) VH_J.getAndBitwiseAndRelease(h, 1L); });
        probe("long.getAndBitwiseXor", () -> { return (long) VH_J.getAndBitwiseXor(h, 1L); });
        probe("long.getAndBitwiseXorAcquire", () -> { return (long) VH_J.getAndBitwiseXorAcquire(h, 1L); });
        probe("long.getAndBitwiseXorRelease", () -> { return (long) VH_J.getAndBitwiseXorRelease(h, 1L); });
        probe("float.getAndAdd", () -> { return (float) VH_F.getAndAdd(h, 1f); });
        probe("float.getAndAddAcquire", () -> { return (float) VH_F.getAndAddAcquire(h, 1f); });
        probe("float.getAndAddRelease", () -> { return (float) VH_F.getAndAddRelease(h, 1f); });
        probe("float.getAndBitwiseOr", () -> { return (float) VH_F.getAndBitwiseOr(h, 1f); });
        probe("float.getAndBitwiseOrAcquire", () -> { return (float) VH_F.getAndBitwiseOrAcquire(h, 1f); });
        probe("float.getAndBitwiseOrRelease", () -> { return (float) VH_F.getAndBitwiseOrRelease(h, 1f); });
        probe("float.getAndBitwiseAnd", () -> { return (float) VH_F.getAndBitwiseAnd(h, 1f); });
        probe("float.getAndBitwiseAndAcquire", () -> { return (float) VH_F.getAndBitwiseAndAcquire(h, 1f); });
        probe("float.getAndBitwiseAndRelease", () -> { return (float) VH_F.getAndBitwiseAndRelease(h, 1f); });
        probe("float.getAndBitwiseXor", () -> { return (float) VH_F.getAndBitwiseXor(h, 1f); });
        probe("float.getAndBitwiseXorAcquire", () -> { return (float) VH_F.getAndBitwiseXorAcquire(h, 1f); });
        probe("float.getAndBitwiseXorRelease", () -> { return (float) VH_F.getAndBitwiseXorRelease(h, 1f); });
        probe("double.getAndAdd", () -> { return (double) VH_D.getAndAdd(h, 1d); });
        probe("double.getAndAddAcquire", () -> { return (double) VH_D.getAndAddAcquire(h, 1d); });
        probe("double.getAndAddRelease", () -> { return (double) VH_D.getAndAddRelease(h, 1d); });
        probe("double.getAndBitwiseOr", () -> { return (double) VH_D.getAndBitwiseOr(h, 1d); });
        probe("double.getAndBitwiseOrAcquire", () -> { return (double) VH_D.getAndBitwiseOrAcquire(h, 1d); });
        probe("double.getAndBitwiseOrRelease", () -> { return (double) VH_D.getAndBitwiseOrRelease(h, 1d); });
        probe("double.getAndBitwiseAnd", () -> { return (double) VH_D.getAndBitwiseAnd(h, 1d); });
        probe("double.getAndBitwiseAndAcquire", () -> { return (double) VH_D.getAndBitwiseAndAcquire(h, 1d); });
        probe("double.getAndBitwiseAndRelease", () -> { return (double) VH_D.getAndBitwiseAndRelease(h, 1d); });
        probe("double.getAndBitwiseXor", () -> { return (double) VH_D.getAndBitwiseXor(h, 1d); });
        probe("double.getAndBitwiseXorAcquire", () -> { return (double) VH_D.getAndBitwiseXorAcquire(h, 1d); });
        probe("double.getAndBitwiseXorRelease", () -> { return (double) VH_D.getAndBitwiseXorRelease(h, 1d); });
        probe("Object.getAndAdd", () -> { return VH_O.getAndAdd(h, "x"); });
        probe("Object.getAndAddAcquire", () -> { return VH_O.getAndAddAcquire(h, "x"); });
        probe("Object.getAndAddRelease", () -> { return VH_O.getAndAddRelease(h, "x"); });
        probe("Object.getAndBitwiseOr", () -> { return VH_O.getAndBitwiseOr(h, "x"); });
        probe("Object.getAndBitwiseOrAcquire", () -> { return VH_O.getAndBitwiseOrAcquire(h, "x"); });
        probe("Object.getAndBitwiseOrRelease", () -> { return VH_O.getAndBitwiseOrRelease(h, "x"); });
        probe("Object.getAndBitwiseAnd", () -> { return VH_O.getAndBitwiseAnd(h, "x"); });
        probe("Object.getAndBitwiseAndAcquire", () -> { return VH_O.getAndBitwiseAndAcquire(h, "x"); });
        probe("Object.getAndBitwiseAndRelease", () -> { return VH_O.getAndBitwiseAndRelease(h, "x"); });
        probe("Object.getAndBitwiseXor", () -> { return VH_O.getAndBitwiseXor(h, "x"); });
        probe("Object.getAndBitwiseXorAcquire", () -> { return VH_O.getAndBitwiseXorAcquire(h, "x"); });
        probe("Object.getAndBitwiseXorRelease", () -> { return VH_O.getAndBitwiseXorRelease(h, "x"); });
        probe("String.getAndAdd", () -> { return (String) VH_T.getAndAdd(h, "x"); });
        probe("String.getAndAddAcquire", () -> { return (String) VH_T.getAndAddAcquire(h, "x"); });
        probe("String.getAndAddRelease", () -> { return (String) VH_T.getAndAddRelease(h, "x"); });
        probe("String.getAndBitwiseOr", () -> { return (String) VH_T.getAndBitwiseOr(h, "x"); });
        probe("String.getAndBitwiseOrAcquire", () -> { return (String) VH_T.getAndBitwiseOrAcquire(h, "x"); });
        probe("String.getAndBitwiseOrRelease", () -> { return (String) VH_T.getAndBitwiseOrRelease(h, "x"); });
        probe("String.getAndBitwiseAnd", () -> { return (String) VH_T.getAndBitwiseAnd(h, "x"); });
        probe("String.getAndBitwiseAndAcquire", () -> { return (String) VH_T.getAndBitwiseAndAcquire(h, "x"); });
        probe("String.getAndBitwiseAndRelease", () -> { return (String) VH_T.getAndBitwiseAndRelease(h, "x"); });
        probe("String.getAndBitwiseXor", () -> { return (String) VH_T.getAndBitwiseXor(h, "x"); });
        probe("String.getAndBitwiseXorAcquire", () -> { return (String) VH_T.getAndBitwiseXorAcquire(h, "x"); });
        probe("String.getAndBitwiseXorRelease", () -> { return (String) VH_T.getAndBitwiseXorRelease(h, "x"); });
        probe("null-recv.boolean.getAndAdd", () -> { return (boolean) VH_Z.getAndAdd((H) null, true); });
        probe("null-recv.boolean.getAndBitwiseOr", () -> { return (boolean) VH_Z.getAndBitwiseOr((H) null, true); });
        probe("null-recv.byte.getAndAdd", () -> { return (byte) VH_B.getAndAdd((H) null, (byte) 1); });
        probe("null-recv.byte.getAndBitwiseOr", () -> { return (byte) VH_B.getAndBitwiseOr((H) null, (byte) 1); });
        probe("null-recv.char.getAndAdd", () -> { return (char) VH_C.getAndAdd((H) null, (char) 1); });
        probe("null-recv.char.getAndBitwiseOr", () -> { return (char) VH_C.getAndBitwiseOr((H) null, (char) 1); });
        probe("null-recv.short.getAndAdd", () -> { return (short) VH_S.getAndAdd((H) null, (short) 1); });
        probe("null-recv.short.getAndBitwiseOr", () -> { return (short) VH_S.getAndBitwiseOr((H) null, (short) 1); });
        probe("null-recv.int.getAndAdd", () -> { return (int) VH_I.getAndAdd((H) null, 1); });
        probe("null-recv.int.getAndBitwiseOr", () -> { return (int) VH_I.getAndBitwiseOr((H) null, 1); });
        probe("null-recv.long.getAndAdd", () -> { return (long) VH_J.getAndAdd((H) null, 1L); });
        probe("null-recv.long.getAndBitwiseOr", () -> { return (long) VH_J.getAndBitwiseOr((H) null, 1L); });
        probe("null-recv.float.getAndAdd", () -> { return (float) VH_F.getAndAdd((H) null, 1f); });
        probe("null-recv.float.getAndBitwiseOr", () -> { return (float) VH_F.getAndBitwiseOr((H) null, 1f); });
        probe("null-recv.double.getAndAdd", () -> { return (double) VH_D.getAndAdd((H) null, 1d); });
        probe("null-recv.double.getAndBitwiseOr", () -> { return (double) VH_D.getAndBitwiseOr((H) null, 1d); });
        probe("null-recv.Object.getAndAdd", () -> { return VH_O.getAndAdd((H) null, "x"); });
        probe("null-recv.Object.getAndBitwiseOr", () -> { return VH_O.getAndBitwiseOr((H) null, "x"); });
        probe("null-recv.String.getAndAdd", () -> { return (String) VH_T.getAndAdd((H) null, "x"); });
        probe("null-recv.String.getAndBitwiseOr", () -> { return (String) VH_T.getAndBitwiseOr((H) null, "x"); });
        probe("control.boolean.getAndSet", () -> { return (boolean) VH_Z.getAndSet(h, true); });
        probe("control.boolean.compareAndSet", () -> { return VH_Z.compareAndSet(h, (boolean) h2.z, true); });
        probe("control.byte.getAndSet", () -> { return (byte) VH_B.getAndSet(h, (byte) 1); });
        probe("control.byte.compareAndSet", () -> { return VH_B.compareAndSet(h, (byte) h2.b, (byte) 1); });
        probe("control.char.getAndSet", () -> { return (char) VH_C.getAndSet(h, (char) 1); });
        probe("control.char.compareAndSet", () -> { return VH_C.compareAndSet(h, (char) h2.c, (char) 1); });
        probe("control.short.getAndSet", () -> { return (short) VH_S.getAndSet(h, (short) 1); });
        probe("control.short.compareAndSet", () -> { return VH_S.compareAndSet(h, (short) h2.s, (short) 1); });
        probe("control.int.getAndSet", () -> { return (int) VH_I.getAndSet(h, 1); });
        probe("control.int.compareAndSet", () -> { return VH_I.compareAndSet(h, (int) h2.i, 1); });
        probe("control.long.getAndSet", () -> { return (long) VH_J.getAndSet(h, 1L); });
        probe("control.long.compareAndSet", () -> { return VH_J.compareAndSet(h, (long) h2.j, 1L); });
        probe("control.float.getAndSet", () -> { return (float) VH_F.getAndSet(h, 1f); });
        probe("control.float.compareAndSet", () -> { return VH_F.compareAndSet(h, (float) h2.f, 1f); });
        probe("control.double.getAndSet", () -> { return (double) VH_D.getAndSet(h, 1d); });
        probe("control.double.compareAndSet", () -> { return VH_D.compareAndSet(h, (double) h2.d, 1d); });
        probe("control.Object.getAndSet", () -> { return VH_O.getAndSet(h, "x"); });
        probe("control.Object.compareAndSet", () -> { return VH_O.compareAndSet(h, h2.o, "x"); });
        probe("control.String.getAndSet", () -> { return (String) VH_T.getAndSet(h, "x"); });
        probe("control.String.compareAndSet", () -> { return VH_T.compareAndSet(h, (String) h2.t, "x"); });

        System.out.println("CK RJdkVarHandleModeSupport checks=" + checks);
        System.out.println("PASS RJdkVarHandleModeSupport");
    }
}
