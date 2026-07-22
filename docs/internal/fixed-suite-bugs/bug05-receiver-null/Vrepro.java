public class Vrepro {
    static class Holder {
        final Object[] scripts;
        boolean a;
        boolean b;
        String sep;
        Holder(Object... scripts) {
            this.scripts = scripts;
        }
        Holder(boolean a, boolean b, String sep, Object[] scripts) {
            this.a = a;
            this.b = b;
            this.sep = sep;
            this.scripts = scripts;
        }
    }
    public static void main(String[] args) {
        Object r1 = new Object();
        Object r2 = new Object();
        Holder h = new Holder(r1, r2);
        System.out.println("h.scripts.length=" + h.scripts.length);
        Holder h2 = new Holder(true, false, ";", new Object[]{r1, r2});
        System.out.println("h2.scripts.length=" + h2.scripts.length + " sep=" + h2.sep);
        System.out.println("OK");
    }
}
