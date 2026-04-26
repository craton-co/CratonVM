public class FieldCheck {
    Object x;
    Object y;
    public FieldCheck(Object a, Object b) { this.x = a; this.y = b; }
    public static void main(String[] args) {
        FieldCheck f = new FieldCheck("aa", "bb");
        System.out.println("x=" + f.x + " y=" + f.y);
    }
}
