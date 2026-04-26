public class TestHang {
    public static void main(String[] args) {
        System.out.println("Step 1: basic string");
        String a = "HELLO";
        String b = "HELLO";
        System.out.println("Step 2: equals");
        boolean eq = a.equals(b);
        System.out.println("Step 3: result = " + eq);

        System.out.println("Step 4: HashSet");
        java.util.HashSet<String> set = new java.util.HashSet<>();
        System.out.println("Step 5: add");
        set.add("QUICKLY");
        System.out.println("Step 6: contains");
        boolean has = set.contains("QUICKLY");
        System.out.println("Step 7: result = " + has);

        System.out.println("Step 8: HashMap");
        java.util.HashMap<String, Integer> map = new java.util.HashMap<>();
        map.put("A", 1);
        System.out.println("Step 9: get");
        Integer val = map.get("A");
        System.out.println("Step 10: val = " + val);

        System.out.println("Step 11: for-each array");
        String[] words = {"THE", "AND", "TO"};
        for (int i = 0; i < words.length; i++) {
            System.out.println("  word: " + words[i]);
        }

        System.out.println("Step 12: String.charAt");
        char c = a.charAt(0);
        System.out.println("Step 13: char = " + c);

        System.out.println("Step 14: toUpperCase");
        String upper = "hello".toUpperCase();
        System.out.println("Step 15: upper = " + upper);

        System.out.println("Done!");
    }
}
