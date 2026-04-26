public class Test8 {
    public static void main(String[] args) {
        System.out.println("t1");
        int fib = fibonacci(10);
        System.out.println("fib=" + fib);
        boolean eq = (fib == 55);
        System.out.println("eq=" + eq);
        if (eq) {
            System.out.println("PASS");
        }

        System.out.println("t2");
        String rev = new StringBuilder("hello").reverse().toString();
        System.out.println("rev=" + rev);

        System.out.println("t3");
        boolean eq2 = "olleh".equals(rev);
        System.out.println("eq2=" + eq2);

        System.out.println("DONE");
    }

    static int fibonacci(int n) {
        if (n <= 1) return n;
        int a = 0, b = 1;
        for (int i = 2; i <= n; i++) {
            int temp = a + b;
            a = b;
            b = temp;
        }
        return b;
    }
}
