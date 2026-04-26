public class TestVarargs {
    static int sum(int... nums) {
        int s = 0;
        for (int i = 0; i < nums.length; i++) {
            s += nums[i];
        }
        return s;
    }

    public static void main(String[] args) {
        System.out.println("start");
        int result = sum(1, 2, 3);
        System.out.println("sum=" + result);
        System.out.println("DONE");
    }
}
