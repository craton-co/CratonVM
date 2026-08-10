import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * The exact text `canAccess`'s IllegalArgumentException messages embed:
 * `"null object for " + member` and `"non-null object for " + member` use
 * Member.toString(). If CratonVM's Field/Method/Constructor toString does not
 * already match HotSpot's, the message cannot match either.
 */
public class MemberToStringProbe {

    public static class Target {
        public int instField = 1;
        public static int staticField = 2;
        private final int privInstField = 3;

        public Target() {
        }

        public void instMethod() {
        }

        public static void staticMethod() {
        }

        protected String withArgs(int a, String[] b) throws java.io.IOException {
            return null;
        }
    }

    public static void main(String[] args) throws Exception {
        Field inst = Target.class.getDeclaredField("instField");
        Field stat = Target.class.getDeclaredField("staticField");
        Field priv = Target.class.getDeclaredField("privInstField");
        Method instM = Target.class.getDeclaredMethod("instMethod");
        Method statM = Target.class.getDeclaredMethod("staticMethod");
        Method argsM = Target.class.getDeclaredMethod("withArgs", int.class, String[].class);
        Constructor<?> ctor = Target.class.getDeclaredConstructor();
        Field foreign = Integer.class.getDeclaredField("value");

        System.out.println("instField   = " + inst);
        System.out.println("staticField = " + stat);
        System.out.println("privField   = " + priv);
        System.out.println("instMethod  = " + instM);
        System.out.println("staticMethod= " + statM);
        System.out.println("withArgs    = " + argsM);
        System.out.println("ctor        = " + ctor);
        System.out.println("Integer.val = " + foreign);
        System.out.println("declaringName = " + Target.class.getName());
        System.out.println("foreignDeclaringName = " + Integer.class.getName());
    }
}
