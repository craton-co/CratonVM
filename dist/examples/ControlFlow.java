public class ControlFlow {
    public static void main(String[] args) {
        // If/else
        int x = 42;
        if (x > 0) {
            System.out.println("Positive");
        } else {
            System.out.println("Non-positive");
        }
        // For loop
        int sum = 0;
        for (int i = 1; i <= 10; i++) {
            sum += i;
        }
        System.out.println("Sum 1..10: " + sum);
        // Switch
        String day = "Monday";
        switch (day) {
            case "Monday": System.out.println("Start of week"); break;
            case "Friday": System.out.println("End of week"); break;
            default: System.out.println("Midweek"); break;
        }
    }
}
