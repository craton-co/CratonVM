public class Test2 {
    public static void main(String[] args) {
        // Test string concat
        String s = "hello" + " " + "world";
        System.out.println("concat: " + s);

        // Test StringBuilder
        StringBuilder sb = new StringBuilder("abcde");
        String rev = sb.reverse().toString();
        System.out.println("reverse: " + rev);

        // Test Math
        double sq = Math.sqrt(144);
        System.out.println("sqrt(144): " + sq);

        // Test exception
        try {
            int x = 10 / 0;
        } catch (ArithmeticException e) {
            System.out.println("caught: " + e.getMessage());
        }

        System.out.println("DONE");
    }
}
