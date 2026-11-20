import java.lang.reflect.*;

/** Sweep 10: what Method.invoke and Constructor.newInstance THROW and SAY. */
public class Sweep10ReflectiveCalls {
    public static class H {
        public int inst(int a) { return a; }
        public static int stat(int a) { return a; }
        public String ref(String s) { return s; }
        public void boom() { throw new IllegalStateException("from the body"); }
        public void boomChecked() throws Exception { throw new Exception("checked"); }
        private int priv() { return 7; }
        public H() { }
        public H(int a) { if (a < 0) throw new IllegalArgumentException("ctor said no"); }
    }
    public abstract static class A { public A() { } }
    interface I { }
    static class Other { }

    interface Call { void run() throws Exception; }
    static void t(String l, Call c) {
        try { c.run(); System.out.println("M " + l + " = no throw"); }
        catch (Throwable x) {
            String cause = (x.getCause() == null) ? "-" : x.getCause().getClass().getName()
                    + "/" + x.getCause().getMessage();
            System.out.println("M " + l + " = " + x.getClass().getName() + " | " + x.getMessage()
                    + " | cause=" + cause);
        }
    }
    static Method m(String n, Class<?>... p) throws Exception { return H.class.getMethod(n, p); }

    public static void main(String[] a) throws Exception {
        H h = new H();

        // ---- argument arity and type ------------------------------------------
        t("too_few", () -> m("inst", int.class).invoke(h));
        t("too_many", () -> m("inst", int.class).invoke(h, 1, 2));
        t("null_args_needed", () -> m("inst", int.class).invoke(h, (Object[]) null));
        t("wrong_arg_type", () -> m("inst", int.class).invoke(h, "x"));
        t("null_for_primitive", () -> m("inst", int.class).invoke(h, (Object) null));
        t("widening_ok", () -> {
            Object r = m("inst", int.class).invoke(h, Byte.valueOf((byte) 3));
            if (!Integer.valueOf(3).equals(r)) throw new AssertionError("r=" + r);
        });
        t("narrowing_bad", () -> m("inst", int.class).invoke(h, Long.valueOf(3)));
        t("null_for_reference_ok", () -> {
            Object r = m("ref", String.class).invoke(h, (Object) null);
            if (r != null) throw new AssertionError("r=" + r);
        });

        // ---- receiver ---------------------------------------------------------
        t("null_receiver", () -> m("inst", int.class).invoke(null, 1));
        t("wrong_receiver", () -> m("inst", int.class).invoke(new Other(), 1));
        t("static_ignores_receiver", () -> {
            Object r = m("stat", int.class).invoke(new Other(), 5);
            if (!Integer.valueOf(5).equals(r)) throw new AssertionError("r=" + r);
        });
        t("static_null_receiver_ok", () -> {
            Object r = m("stat", int.class).invoke(null, 5);
            if (!Integer.valueOf(5).equals(r)) throw new AssertionError("r=" + r);
        });

        // ---- the body throws: InvocationTargetException wrapping ---------------
        t("body_unchecked", () -> m("boom").invoke(h));
        t("body_checked", () -> m("boomChecked").invoke(h));
        t("body_error", () -> {
            Method mm = H.class.getMethod("boom");
            try { mm.invoke(h); } catch (InvocationTargetException e) {
                throw new IllegalStateException("wrapped=" + e.getCause().getClass().getName()
                        + " msg=" + e.getCause().getMessage() + " own=" + e.getMessage());
            }
        });

        // ---- access -----------------------------------------------------------
        t("private_no_access", () -> H.class.getDeclaredMethod("priv").invoke(h));
        t("private_with_access", () -> {
            Method mm = H.class.getDeclaredMethod("priv");
            mm.setAccessible(true);
            if (!Integer.valueOf(7).equals(mm.invoke(h))) throw new AssertionError();
        });

        // ---- Constructor.newInstance ------------------------------------------
        t("ctor_wrong_arity", () -> H.class.getConstructor(int.class).newInstance());
        t("ctor_wrong_type", () -> H.class.getConstructor(int.class).newInstance("x"));
        t("ctor_null_primitive", () -> H.class.getConstructor(int.class)
                .newInstance((Object) null));
        t("ctor_body_throws", () -> H.class.getConstructor(int.class).newInstance(-1));
        t("ctor_abstract", () -> A.class.getConstructor().newInstance());
        t("ctor_ok", () -> {
            Object o = H.class.getConstructor(int.class).newInstance(1);
            if (!(o instanceof H)) throw new AssertionError();
        });

        // ---- java.lang.reflect.Array -------------------------------------------
        int[] arr = new int[] {1, 2};
        t("array_get_oob", () -> Array.get(arr, 5));
        t("array_get_not_array", () -> Array.get("notArray", 0));
        t("array_get_null", () -> Array.get(null, 0));
        t("array_set_wrong_type", () -> Array.set(arr, 0, "x"));
        t("array_setInt_on_ref", () -> Array.setInt(new String[1], 0, 1));
        t("array_newInstance_negative", () -> Array.newInstance(int.class, -1));
        t("array_newInstance_void", () -> Array.newInstance(void.class, 1));
        t("array_getLength_not_array", () -> Array.getLength("x"));
    }
}
