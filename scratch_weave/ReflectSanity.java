import java.lang.reflect.Constructor;
import java.lang.reflect.Method;

public class ReflectSanity {
    static class Base {
        int v;
        public Base() { this.v = 1; }
        public Base(int x) { this.v = x; }
        public String who() { return "Base:" + v; }
    }
    static class Sub extends Base {
        public Sub() { super(7); }
        @Override public String who() { return "Sub:" + v; }
    }

    public static void main(String[] args) throws Exception {
        int fails = 0;

        // 1. Virtual dispatch via Method.invoke: Base.who on a Sub receiver -> Sub.who
        Method who = Base.class.getMethod("who");
        Object sub = Sub.class.getConstructor().newInstance();
        String r1 = (String) who.invoke(sub);
        if (!r1.equals("Sub:7")) { System.out.println("FAIL1 got " + r1); fails++; }

        // 2. Base no-arg constructor newInstance
        Object b0 = Base.class.getConstructor().newInstance();
        String r2 = (String) who.invoke(b0);
        if (!r2.equals("Base:1")) { System.out.println("FAIL2 got " + r2); fails++; }

        // 3. Constructor with int arg
        Constructor<Base> c = Base.class.getConstructor(int.class);
        Object b5 = c.newInstance(5);
        String r3 = (String) who.invoke(b5);
        if (!r3.equals("Base:5")) { System.out.println("FAIL3 got " + r3); fails++; }

        // 4. Class.newInstance (deprecated) on Sub
        @SuppressWarnings("deprecation")
        Object s2 = Sub.class.newInstance();
        String r4 = (String) who.invoke(s2);
        if (!r4.equals("Sub:7")) { System.out.println("FAIL4 got " + r4); fails++; }

        // 5. invoke directly on Sub.who via Sub mirror
        Method who2 = Sub.class.getMethod("who");
        String r5 = (String) who2.invoke(sub);
        if (!r5.equals("Sub:7")) { System.out.println("FAIL5 got " + r5); fails++; }

        // 6. String length via reflection (JDK class, single loader)
        Method len = String.class.getMethod("length");
        int l = (Integer) len.invoke("hello");
        if (l != 5) { System.out.println("FAIL6 got " + l); fails++; }

        System.out.println(fails == 0 ? "SANITY: PASS" : ("SANITY: FAIL (" + fails + ")"));
    }
}
