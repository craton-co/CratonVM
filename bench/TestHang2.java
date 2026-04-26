public class TestHang2 {
    public static void main(String[] args) {
        System.out.println("A: Integer.valueOf");
        Integer v = Integer.valueOf(1);
        System.out.println("B: v = " + v);

        System.out.println("C: HashMap new");
        java.util.HashMap<String, Integer> map = new java.util.HashMap<>();
        System.out.println("D: hashCode");
        int h = "A".hashCode();
        System.out.println("E: hash = " + h);

        System.out.println("F: put");
        map.put("A", v);
        System.out.println("G: done");
    }
}
