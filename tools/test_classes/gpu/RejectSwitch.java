public class RejectSwitch {
    // A tableswitch / lookupswitch in the bytecode triggers
    // Reject(Switch). Compact ints + default => tableswitch.
    public static int pick(int x) {
        switch (x) {
            case 0: return 10;
            case 1: return 20;
            case 2: return 30;
            case 3: return 40;
            default: return -1;
        }
    }
}
