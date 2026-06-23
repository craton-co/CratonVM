import java.lang.reflect.Method;
import java.util.*;

public class Vrepro3 {
    interface Pop { void go(); }
    public static class RDP implements Pop {
        List<Object> scripts = new ArrayList<>();
        private String sep = ";";
        private boolean a = false;
        public RDP(Object... scripts) {
            setScripts(scripts);
        }
        public void setScripts(Object[] s) {
            this.scripts = new ArrayList<>(Arrays.asList(s));
        }
        public void go() {}
    }
    static Object s1 = new Object(), s2 = new Object();
    // invoked reflectively -> goes through execute() eager first-call compile
    public static void constructWithMultipleResources() {
        RDP r = new RDP(s1, s2);
        if (r.scripts.size() != 2) throw new RuntimeException("size=" + r.scripts.size());
        System.out.println("scripts.size=" + r.scripts.size());
    }
    public static void main(String[] args) throws Exception {
        Method m = Vrepro3.class.getMethod("constructWithMultipleResources");
        m.invoke(null);
        System.out.println("OK");
    }
}
