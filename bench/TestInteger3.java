public class TestInteger3 {
    public static void main(String[] args) {
        System.out.println("Creating Integer");
        Integer i = Integer.valueOf(42);
        System.out.println("Created");
        if (i != null) {
            System.out.println("Not null");
            int v = i.intValue();
            System.out.println("Value: " + v);
        } else {
            System.out.println("NULL!");
        }
    }
}
