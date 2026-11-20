public class RejectTypeCheck {
    // `instanceof` (0xC1) triggers Reject(TypeCheck). We funnel the
    // int[] parameter through an Object local so javac emits the
    // instanceof opcode against int[]. The method body has no
    // allocation, no calls, and no exception handlers — leaving the
    // instanceof as the analyzer's first reject hit.
    public static int isIntArray(int[] a) {
        Object o = a;
        return (o instanceof int[]) ? 1 : 0;
    }
}
