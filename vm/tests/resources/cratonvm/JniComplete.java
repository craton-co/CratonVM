package cratonvm;

/**
 * Session 44: JNI Completeness — test class.
 *
 * Each static method returns an int so the test harness can verify execution.
 * These methods exercise code paths relevant to JNI: field access, method calls,
 * arrays, strings, arithmetic, control flow, object creation, and more.
 */
public class JniComplete {

    // --- Static fields for JNI field access tests ---
    static int staticIntField = 100;
    static long staticLongField = 200L;
    static String staticStringField = "hello";

    // --- Instance helper class ---
    private int value;

    public JniComplete(int v) {
        this.value = v;
    }

    public int getValue() {
        return value;
    }

    public void setValue(int v) {
        this.value = v;
    }

    // --- Static field access ---
    public static int static_int_field() {
        return staticIntField; // 100
    }

    public static int static_long_field() {
        return (int) staticLongField; // 200
    }

    public static int static_string_field_len() {
        return staticStringField.length(); // 5
    }

    // --- Instance field and method access ---
    public static int instance_create_get() {
        JniComplete obj = new JniComplete(42);
        return obj.getValue(); // 42
    }

    public static int instance_set_get() {
        JniComplete obj = new JniComplete(0);
        obj.setValue(99);
        return obj.getValue(); // 99
    }

    // --- Array operations ---
    public static int array_int_create() {
        int[] arr = new int[10];
        return arr.length; // 10
    }

    public static int array_int_set_get() {
        int[] arr = new int[5];
        arr[0] = 10;
        arr[1] = 20;
        arr[2] = 30;
        arr[3] = 40;
        arr[4] = 50;
        return arr[2]; // 30
    }

    public static int array_int_sum() {
        int[] arr = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10};
        int sum = 0;
        for (int i = 0; i < arr.length; i++) {
            sum += arr[i];
        }
        return sum; // 55
    }

    public static int array_long_ops() {
        long[] arr = new long[3];
        arr[0] = 100L;
        arr[1] = 200L;
        arr[2] = 300L;
        return (int)(arr[0] + arr[1] + arr[2]); // 600
    }

    public static int array_byte_ops() {
        byte[] arr = new byte[4];
        arr[0] = 10;
        arr[1] = 20;
        arr[2] = 30;
        arr[3] = 40;
        return arr[0] + arr[1] + arr[2] + arr[3]; // 100
    }

    public static int array_boolean_ops() {
        boolean[] arr = new boolean[3];
        arr[0] = true;
        arr[1] = false;
        arr[2] = true;
        int count = 0;
        for (int i = 0; i < arr.length; i++) {
            if (arr[i]) count++;
        }
        return count; // 2
    }

    public static int array_double_ops() {
        double[] arr = new double[3];
        arr[0] = 1.5;
        arr[1] = 2.5;
        arr[2] = 3.0;
        return (int)(arr[0] + arr[1] + arr[2]); // 7
    }

    public static int array_object_ops() {
        String[] arr = new String[3];
        arr[0] = "Hello";
        arr[1] = " ";
        arr[2] = "World";
        int len = 0;
        for (int i = 0; i < arr.length; i++) {
            len += arr[i].length();
        }
        return len; // 11
    }

    // --- String operations ---
    public static int string_new_utf() {
        String s = "Hello JNI";
        return s.length(); // 9
    }

    public static int string_concat() {
        String a = "Hello";
        String b = " World";
        String c = a + b;
        return c.length(); // 11
    }

    public static int string_char_at() {
        String s = "ABCDEF";
        return s.charAt(2); // 'C' = 67
    }

    public static int string_index_of() {
        String s = "Hello World";
        return s.indexOf('W'); // 6
    }

    // --- Object creation and reference handling ---
    public static int object_alloc() {
        Object obj = new Object();
        return (obj != null) ? 1 : 0; // 1
    }

    public static int object_class_check() {
        JniComplete obj = new JniComplete(5);
        return (obj instanceof JniComplete) ? 1 : 0; // 1
    }

    public static int object_null_check() {
        Object obj = null;
        return (obj == null) ? 1 : 0; // 1
    }

    // --- Method call types ---
    private static int staticHelper(int a, int b) {
        return a * b;
    }

    public static int call_static_method() {
        return staticHelper(6, 7); // 42
    }

    private int instanceHelper(int x) {
        return this.value + x;
    }

    public static int call_instance_method() {
        JniComplete obj = new JniComplete(10);
        return obj.instanceHelper(32); // 42
    }

    // --- Arithmetic helpers ---
    public static int arith_add() {
        int a = Integer.MAX_VALUE;
        int b = 1;
        // Overflow wraps around
        return (a + b == Integer.MIN_VALUE) ? 1 : 0; // 1
    }

    public static int arith_long_math() {
        long a = 1000000000L;
        long b = 2000000000L;
        long c = a + b;
        return (int)(c / 1000000000L); // 3
    }

    public static int arith_float_cast() {
        float f = 3.14f;
        return (int)(f * 10); // 31
    }

    public static int arith_double_cast() {
        double d = 2.718;
        return (int)(d * 100); // 271
    }

    // --- Exception handling ---
    public static int exception_throw_catch() {
        try {
            throw new RuntimeException("test");
        } catch (RuntimeException e) {
            return 1; // 1
        }
    }

    // --- Monitor (synchronization) ---
    public static int monitor_basic() {
        Object lock = new Object();
        int result = 0;
        synchronized (lock) {
            result = 42;
        }
        return result; // 42
    }

    // --- Multi-dimensional arrays ---
    public static int array_2d() {
        int[][] matrix = new int[3][3];
        int count = 0;
        for (int i = 0; i < 3; i++) {
            for (int j = 0; j < 3; j++) {
                matrix[i][j] = count++;
            }
        }
        return matrix[1][2]; // 5
    }

    // --- Complex: Fibonacci iterative ---
    public static int fib_iterative() {
        int n = 15;
        int a = 0, b = 1;
        for (int i = 2; i <= n; i++) {
            int t = a + b;
            a = b;
            b = t;
        }
        return b; // 610
    }
}
