import java.util.*;

public class Vrepro2 {
    interface Pop { void go(); }
    static class RDP implements Pop {
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
    static int build(Object s1, Object s2) {
        RDP r = new RDP(s1, s2);
        return r.scripts.size();
    }
    public static void main(String[] args) {
        Object s1 = new Object(), s2 = new Object();
        long sum = 0;
        for (int i = 0; i < 2_000_000; i++) {
            sum += build(s1, s2);
        }
        System.out.println("sum=" + sum);
        System.out.println("OK");
    }
}
