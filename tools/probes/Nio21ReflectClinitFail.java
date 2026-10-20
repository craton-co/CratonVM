import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

/**
 * nio21 wave 21: reflective static calls that throw, around the settling
 * static door (CRATONVM_STATIC_SETTLE_ON_THROW). A planned static target whose
 * call threw may now settle; that must never happen for a class whose
 * {@code <clinit>} failed: HotSpot answers ExceptionInInitializerError the
 * first time and NoClassDefFoundError every time after.
 *
 * Conformance only: prints answers, no timing.
 */
public class Nio21ReflectClinitFail {
    static class Bad {
        static int seen;
        static {
            if (Boolean.parseBoolean("true")) {
                throw new IllegalStateException("bad clinit");
            }
        }
        static int f(int x) {
            return x + seen;
        }
    }

    static class Thrower {
        static int calls;
        static int t(int x) {
            calls++;
            if (x >= 0) {
                throw new IllegalArgumentException("t" + (x % 3));
            }
            return x;
        }
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (sb.length() > 0) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getName()).append(": ").append(c.getMessage());
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        Method bad = Bad.class.getDeclaredMethod("f", int.class);
        for (int i = 0; i < 4; i++) {
            try {
                Object r = bad.invoke(null, i);
                System.out.println("bad " + i + " returned " + r);
            } catch (Throwable t) {
                System.out.println("bad " + i + " " + describe(t));
            }
        }
        Method thr = Thrower.class.getDeclaredMethod("t", int.class);
        int ite = 0;
        String first = null;
        String last = null;
        for (int i = 0; i < 20000; i++) {
            try {
                thr.invoke(null, i);
            } catch (InvocationTargetException e) {
                ite++;
                String d = describe(e);
                if (first == null) {
                    first = d;
                }
                last = d;
            }
        }
        System.out.println("thrower ite=" + ite + " calls=" + Thrower.calls);
        System.out.println("thrower first " + first);
        System.out.println("thrower last " + last);
        System.out.println("thrower ok " + thr.invoke(null, -5));
        try {
            thr.invoke(null, 7);
        } catch (InvocationTargetException e) {
            StackTraceElement[] st = e.getCause().getStackTrace();
            System.out.println("thrower trace0 " + st[0].getClassName() + "." + st[0].getMethodName());
        }
        for (int i = 0; i < 2; i++) {
            try {
                bad.invoke(null, 1);
            } catch (Throwable t) {
                System.out.println("bad again " + describe(t));
            }
        }
    }
}
