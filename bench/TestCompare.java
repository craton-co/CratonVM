public class TestCompare {
    public static void main(String[] args) {
        // Direct compareTo
        int c1 = "Alice".compareTo("Bob");
        int c2 = "Bob".compareTo("Alice");
        int c3 = "Alice".compareTo("Alice");
        System.out.println("Alice vs Bob: " + c1);  // should be negative
        System.out.println("Bob vs Alice: " + c2);  // should be positive
        System.out.println("Alice vs Alice: " + c3); // should be 0

        // Via Comparator lambda
        java.util.Comparator<String> cmp = (a, b) -> a.compareTo(b);
        int r1 = cmp.compare("Alice", "Bob");
        int r2 = cmp.compare("Bob", "Alice");
        System.out.println("cmp Alice vs Bob: " + r1);
        System.out.println("cmp Bob vs Alice: " + r2);

        System.out.println("DONE");
    }
}
