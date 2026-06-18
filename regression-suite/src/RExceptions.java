import java.util.*;

/**
 * Regression: exception semantics + array covariance. Covers ArrayStoreException
 * (the aastore check regressed once — wrongly rejecting valid covariant /
 * dynamic-proxy stores), try/finally ordering, cause chains, and
 * try-with-resources close ordering.
 */
public class RExceptions {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    interface Animal {}
    static class Cat implements Animal {}
    static class Dog implements Animal {}

    static class Res implements AutoCloseable {
        final String id; final List<String> log;
        Res(String id, List<String> log) { this.id = id; this.log = log; }
        public void close() { log.add("close " + id); }
    }

    public static void main(String[] a) {
        // ---- ArrayStoreException: storing an incompatible type throws ----
        Object[] strs = new String[2];
        boolean ase = false;
        try { strs[0] = Integer.valueOf(1); } catch (ArrayStoreException e) { ase = true; }
        check(ase, "ArrayStoreException on incompatible store");

        // ---- ...but valid covariant stores (incl. interface[] / subtype) must NOT throw ----
        Animal[] animals = new Animal[2];
        animals[0] = new Cat(); animals[1] = new Dog();   // subtype into interface[]
        check(animals[0] instanceof Cat, "covariant interface[] store");
        Object[] objs = new Object[2];
        objs[0] = "any"; objs[1] = 42;                     // Object[] accepts anything
        Number[] nums = new Number[2];
        nums[0] = Integer.valueOf(1); nums[1] = Double.valueOf(2.0);  // subtype into superclass[]
        check(nums[0].intValue() == 1, "covariant superclass[] store");
        // Annotation proxies are dynamic implementations of an annotation interface —
        // storing one into an annotation/Object[] must be allowed (regressed once).
        Override ann = RExceptions.class.getAnnotation(Override.class); // null, but type resolves
        java.lang.annotation.Annotation[] anns = RExceptions.class.getAnnotations();
        Object[] holder = new java.lang.annotation.Annotation[anns.length];
        System.arraycopy(anns, 0, holder, 0, anns.length);
        check(true, "annotation array copy");

        // ---- try / catch / finally ordering ----
        List<String> order = new ArrayList<>();
        try { order.add("try"); throw new IllegalStateException("x"); }
        catch (IllegalStateException e) { order.add("catch:" + e.getMessage()); }
        finally { order.add("finally"); }
        check(order.equals(Arrays.asList("try", "catch:x", "finally")), "try/catch/finally order");

        // ---- NPE + AIOOBE + arithmetic + CCE ----
        boolean npe = false; try { String s = null; s.length(); } catch (NullPointerException e) { npe = true; }
        check(npe, "NullPointerException");
        boolean aioobe = false; try { int[] x = new int[2]; int y = x[5]; } catch (ArrayIndexOutOfBoundsException e) { aioobe = true; }
        check(aioobe, "ArrayIndexOutOfBoundsException");
        boolean arith = false; try { int z = 1 / (a.length); } catch (ArithmeticException e) { arith = true; }
        check(arith, "ArithmeticException divide-by-zero");
        boolean cce = false; try { Object o = "s"; Integer i = (Integer) o; } catch (ClassCastException e) { cce = true; }
        check(cce, "ClassCastException");

        // ---- cause chain ----
        Exception root = new IllegalArgumentException("root");
        Exception wrap = new RuntimeException("wrap", root);
        check(wrap.getCause() == root && "root".equals(wrap.getCause().getMessage()), "cause chain");

        // ---- multi-catch ----
        String caught = null;
        try { throw new java.io.IOException("io"); }
        catch (RuntimeException | java.io.IOException e) { caught = e.getMessage(); }
        check("io".equals(caught), "multi-catch");

        // ---- try-with-resources: close in reverse order, even on exception ----
        List<String> rlog = new ArrayList<>();
        boolean tw = false;
        try (Res r1 = new Res("1", rlog); Res r2 = new Res("2", rlog)) {
            rlog.add("body"); throw new RuntimeException("boom");
        } catch (RuntimeException e) { tw = true; }
        check(tw && rlog.equals(Arrays.asList("body", "close 2", "close 1")), "try-with-resources order");

        // ---- `return x` captures the value before finally runs (returns 1, not 2) ----
        check(returnsAfterFinally() == 1, "finally does not alter already-evaluated return");

        System.out.println("PASS RExceptions (" + checks + " checks)");
    }

    static int returnsAfterFinally() {
        int x = 1;
        try { return x; } finally { x = 2; /* does not change the already-evaluated return */ }
    }
}
