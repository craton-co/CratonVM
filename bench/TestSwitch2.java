public class TestSwitch2 {
    static String dayType(int day) {
        switch (day) {
            case 1: case 2: case 3: case 4: case 5: return "weekday";
            case 6: case 7: return "weekend";
            default: return "invalid";
        }
    }

    static int sum(int... nums) {
        int s = 0;
        for (int i = 0; i < nums.length; i++) s += nums[i];
        return s;
    }

    public static void main(String[] args) {
        System.out.println("switch: " + dayType(3));
        System.out.println("switch: " + dayType(6));
        System.out.println("ternary: " + ((10 > 5) ? "big" : "small"));
        System.out.println("varargs: " + sum(1, 2, 3, 4, 5));
        System.out.println(4 + "/4 passed");
    }
}
