public class TestEnum {
    enum Color { RED, GREEN, BLUE }
    enum Season { SPRING, SUMMER, AUTUMN, WINTER }

    public static void main(String[] args) {
        int pass = 0;

        // 1. Enum ordinal and name
        Color c = Color.GREEN;
        if (c.ordinal() == 1) { System.out.println("PASS: ordinal"); pass++; }
        else System.out.println("FAIL: ordinal=" + c.ordinal());

        if ("GREEN".equals(c.name())) { System.out.println("PASS: name"); pass++; }
        else System.out.println("FAIL: name=" + c.name());

        // 2. Enum switch
        String label = switch (c) {
            case RED -> "red";
            case GREEN -> "green";
            case BLUE -> "blue";
        };
        if ("green".equals(label)) { System.out.println("PASS: enum switch"); pass++; }
        else System.out.println("FAIL: switch=" + label);

        // 3. Enum valueOf
        Color blue = Color.valueOf("BLUE");
        if (blue.ordinal() == 2) { System.out.println("PASS: valueOf"); pass++; }
        else System.out.println("FAIL: valueOf=" + blue.ordinal());

        // 4. Enum values()
        Color[] all = Color.values();
        if (all.length == 3) { System.out.println("PASS: values"); pass++; }
        else System.out.println("FAIL: values.length=" + all.length);

        System.out.println(pass + "/5 passed");
    }
}
