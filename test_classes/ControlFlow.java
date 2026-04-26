public class ControlFlow {
    public static void main(String[] args) {
        // If/else
        int x = 42;
        if (x > 40) {
            System.out.println("x > 40: OK");
        } else {
            System.out.println("ERROR: x should be > 40");
        }

        // For loop
        int sum = 0;
        for (int i = 1; i <= 10; i++) {
            sum += i;
        }
        System.out.println(sum); // 55

        // While loop
        int count = 0;
        while (count < 5) {
            count++;
        }
        System.out.println(count); // 5

        // Switch
        int day = 3;
        switch (day) {
            case 1: System.out.println("Monday"); break;
            case 2: System.out.println("Tuesday"); break;
            case 3: System.out.println("Wednesday"); break;
            default: System.out.println("Other"); break;
        }

        System.out.println("All control flow tests passed!");
    }
}
