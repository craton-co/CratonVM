import java.util.*;

public class TestInterface {
    interface Greeting {
        String greet(String name);
        default String shout(String name) { return greet(name).toUpperCase(); }
    }

    static class FormalGreeting implements Greeting {
        public String greet(String name) { return "Good day, " + name; }
    }

    static class CasualGreeting implements Greeting {
        public String greet(String name) { return "Hey " + name + "!"; }
    }

    interface Printable {
        void print();
    }

    // Multiple interface implementation
    static class Doc implements Greeting, Printable {
        String content;
        Doc(String c) { content = c; }
        public String greet(String name) { return content + " for " + name; }
        public void print() { System.out.println("  doc: " + content); }
    }

    public static void main(String[] args) {
        int pass = 0;

        // 1. Interface dispatch
        Greeting g1 = new FormalGreeting();
        if (g1.greet("Alice").contains("Good day")) { System.out.println("PASS: interface"); pass++; }
        else System.out.println("FAIL: greet=" + g1.greet("Alice"));

        // 2. Polymorphic dispatch
        List<Greeting> greetings = new ArrayList<>();
        greetings.add(new FormalGreeting());
        greetings.add(new CasualGreeting());
        String all = "";
        for (int i = 0; i < greetings.size(); i++) {
            all += greetings.get(i).greet("Bob") + "; ";
        }
        if (all.contains("Good day") && all.contains("Hey")) {
            System.out.println("PASS: polymorphism"); pass++;
        } else System.out.println("FAIL: poly=" + all);

        // 3. Default method
        Greeting g2 = new CasualGreeting();
        String shouted = g2.shout("Eve");
        if ("HEY EVE!".equals(shouted)) { System.out.println("PASS: default method"); pass++; }
        else System.out.println("FAIL: shout=" + shouted);

        // 4. Multiple interfaces
        Doc doc = new Doc("Report");
        Greeting docGreet = doc;
        Printable docPrint = doc;
        if (docGreet.greet("X").contains("Report")) {
            System.out.println("PASS: multi-interface"); pass++;
        } else System.out.println("FAIL: multi");

        System.out.println(pass + "/4 passed");
    }
}
