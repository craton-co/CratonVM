public class TestCompare3 {
    // Make compareTo call non-trivial to avoid JIT
    static int myCompare(String a, String b) {
        int result = a.compareTo(b);
        return result;
    }

    public static void main(String[] args) {
        java.util.Comparator<String> cmp = (a, b) -> myCompare(a, b);
        System.out.println("Alice vs Bob: " + cmp.compare("Alice", "Bob"));
        System.out.println("Bob vs Alice: " + cmp.compare("Bob", "Alice"));

        // Also test with Comparator.naturalOrder()
        java.util.Comparator<String> nat = java.util.Comparator.naturalOrder();
        System.out.println("nat Alice vs Bob: " + nat.compare("Alice", "Bob"));

        System.out.println("DONE");
    }
}
