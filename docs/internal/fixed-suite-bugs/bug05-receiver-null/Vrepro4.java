import java.util.*;
public class Vrepro4 {
    public static class RDP {
        List<Object> scripts = new ArrayList<>();
        private String sep = ";";
        public RDP(Object... scripts) { setScripts(scripts); }
        public void setScripts(Object[] s) { this.scripts = new ArrayList<>(Arrays.asList(s)); }
    }
    static Object s1 = new Object(), s2 = new Object();
    // hot caller: bg worker will compile this -> exercises analyze_escapes on the varargs new
    static int build() {
        RDP r = new RDP(s1, s2);
        return r.scripts.size();
    }
    public static void main(String[] args) {
        long sum = 0;
        for (int i = 0; i < 200000; i++) sum += build();
        System.out.println("sum=" + sum + " (expect 400000)");
        System.out.println(sum == 400000 ? "OK" : "FAIL");
    }
}
