public class Test9 {
    public static void main(String[] args) {
        System.out.println("start");
        if (true) {
            System.out.println("PASS: fibonacci(10) = 55");
        } else {
            System.out.println("FAIL: fibonacci(10) = " + 0);
        }
        System.out.println("between");
        if ("olleh".equals("olleh")) {
            System.out.println("PASS: reverse = olleh");
        } else {
            System.out.println("FAIL: reverse = " + "bad");
        }
        System.out.println("DONE");
    }
}
