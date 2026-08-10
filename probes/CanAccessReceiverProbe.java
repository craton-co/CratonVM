import java.lang.reflect.AccessibleObject;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * The full receiver-argument matrix of AccessibleObject.canAccess(Object),
 * printed as either `=<boolean>` or `!<ExceptionSimpleName>: <message>`.
 *
 * Every cell states what HotSpot does BEFORE any access check happens, so the
 * static/non-static x null/wrong-type/right-type grid is what has to be
 * matched, not just the one cell the bug report names.
 */
public class CanAccessReceiverProbe {

    public static class Target {
        public int instField = 1;
        public static int staticField = 2;
        private int privInstField = 3;

        public Target() {
        }

        public void instMethod() {
        }

        public static void staticMethod() {
        }

        private void privInstMethod() {
        }
    }

    public static class Sub extends Target {
    }

    public static class Unrelated {
    }

    public static void main(String[] args) throws Exception {
        Target t = new Target();
        Sub s = new Sub();
        Unrelated u = new Unrelated();

        Field inst = Target.class.getDeclaredField("instField");
        Field stat = Target.class.getDeclaredField("staticField");
        Field privInst = Target.class.getDeclaredField("privInstField");
        Method instM = Target.class.getDeclaredMethod("instMethod");
        Method statM = Target.class.getDeclaredMethod("staticMethod");
        Method privInstM = Target.class.getDeclaredMethod("privInstMethod");
        Constructor<?> ctor = Target.class.getDeclaredConstructor();

        row("instField      obj=null    ", inst, null);
        row("instField      obj=Target  ", inst, t);
        row("instField      obj=Sub     ", inst, s);
        row("instField      obj=Unrelated", inst, u);
        row("instField      obj=String  ", inst, "x");

        row("staticField    obj=null    ", stat, null);
        row("staticField    obj=Target  ", stat, t);
        row("staticField    obj=Unrelated", stat, u);

        row("privInstField  obj=null    ", privInst, null);
        row("privInstField  obj=Target  ", privInst, t);
        row("privInstField  obj=Unrelated", privInst, u);

        row("instMethod     obj=null    ", instM, null);
        row("instMethod     obj=Target  ", instM, t);
        row("instMethod     obj=Sub     ", instM, s);
        row("instMethod     obj=Unrelated", instM, u);

        row("staticMethod   obj=null    ", statM, null);
        row("staticMethod   obj=Target  ", statM, t);

        row("privInstMethod obj=null    ", privInstM, null);
        row("privInstMethod obj=Target  ", privInstM, t);

        row("ctor           obj=null    ", ctor, null);
        row("ctor           obj=Target  ", ctor, t);
        row("ctor           obj=Unrelated", ctor, u);

        // A member the caller must NOT reach, so the receiver checks are shown
        // to run BEFORE the access decision rather than after it.
        Field foreignInst = Integer.class.getDeclaredField("value");
        row("Integer.value  obj=null    ", foreignInst, null);
        row("Integer.value  obj=Integer ", foreignInst, Integer.valueOf(7));
        row("Integer.value  obj=String  ", foreignInst, "x");

        // setAccessible(true) must not suppress the argument validation.
        Field opened = Target.class.getDeclaredField("privInstField");
        opened.setAccessible(true);
        row("opened priv    obj=null    ", opened, null);
        row("opened priv    obj=Target  ", opened, t);
        row("opened priv    obj=Unrelated", opened, u);
    }

    static void row(String label, AccessibleObject ao, Object obj) {
        String out;
        try {
            out = "=" + ao.canAccess(obj);
        } catch (Throwable e) {
            out = "!" + e.getClass().getSimpleName() + ": " + e.getMessage();
        }
        System.out.println(label + " -> " + out);
    }
}
