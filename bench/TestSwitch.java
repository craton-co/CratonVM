public class TestSwitch {
    static String dayType(int day) {
        switch (day) {
            case 1: case 2: case 3: case 4: case 5: return "weekday";
            case 6: case 7: return "weekend";
            default: return "invalid";
        }
    }

    static String grade(int score) {
        if (score >= 90) return "A";
        else if (score >= 80) return "B";
        else if (score >= 70) return "C";
        else return "F";
    }

    public static void main(String[] args) {
        int pass = 0;

        if ("weekday".equals(dayType(3))) { System.out.println("PASS: switch weekday"); pass++; }
        if ("weekend".equals(dayType(6))) { System.out.println("PASS: switch weekend"); pass++; }
        if ("invalid".equals(dayType(0))) { System.out.println("PASS: switch default"); pass++; }
        if ("A".equals(grade(95))) { System.out.println("PASS: grade A"); pass++; }
        if ("C".equals(grade(75))) { System.out.println("PASS: grade C"); pass++; }

        // Try-with-resources simulation (using try-finally since AutoCloseable needs more)
        String result = "none";
        try {
            result = "opened";
            if (true) result = "used";
        } finally {
            result = result + "+closed";
        }
        if ("used+closed".equals(result)) { System.out.println("PASS: try-finally"); pass++; }

        // Ternary
        int x = 10;
        String s = (x > 5) ? "big" : "small";
        if ("big".equals(s)) { System.out.println("PASS: ternary"); pass++; }

        // Varargs
        int sum = sum(1, 2, 3, 4, 5);
        if (sum == 15) { System.out.println("PASS: varargs"); pass++; }

        System.out.println(pass + "/8 passed");
    }

    static int sum(int... nums) {
        int s = 0;
        for (int n : nums) s += n;
        return s;
    }
}
