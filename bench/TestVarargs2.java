public class TestVarargs2 {
    static int sum(int... nums) {
        int s = 0;
        for (int i = 0; i < nums.length; i++) s += nums[i];
        return s;
    }

    public static void main(String[] args) {
        System.out.println("3 args: " + sum(1, 2, 3));
        System.out.println("5 args: " + sum(1, 2, 3, 4, 5));
        System.out.println("DONE");
    }
}
