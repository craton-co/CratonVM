public class Main {
    public static void main(String[] args) {
        B b = new B();
        Object o = b.x;
        System.out.println("b.x = " + o);
        System.out.println("A.x-object".equals(o) ? "PROBE-PASS" : "PROBE-FAIL got=" + o);
    }
}
