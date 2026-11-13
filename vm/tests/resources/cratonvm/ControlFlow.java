package cratonvm;

/**
 * Test control flow: if/else, loops, switch.
 */
public class ControlFlow {
    public static int testIfElse() {
        int x = 10;
        if (x > 5) {
            return 1;
        } else {
            return 0;
        }
    }

    public static int testLoop() {
        int sum = 0;
        for (int i = 0; i < 10; i++) {
            sum += i;
        }
        return sum;  // 0+1+2+...+9 = 45
    }

    public static int testWhile() {
        int n = 10;
        int result = 1;
        while (n > 0) {
            result *= 2;
            n--;
        }
        return result;  // 2^10 = 1024
    }

    public static int testSwitch(int x) {
        switch (x) {
            case 1: return 10;
            case 2: return 20;
            case 3: return 30;
            default: return -1;
        }
    }
}
