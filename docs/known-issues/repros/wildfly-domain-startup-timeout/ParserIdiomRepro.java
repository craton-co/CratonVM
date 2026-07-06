public class ParserIdiomRepro {
    int[] intStack = new int[64];
    int intPtr = 50;

    char[][] identifierStack = new char[8][];
    char[][] identifierStackDst = new char[8][];

    long mism = 0;
    long calls = 0;

    ParserIdiomRepro() {
        for (int k = 0; k < identifierStack.length; k++) {
            identifierStack[k] = new char[] {'a', 'b', 'c'};
        }
    }

    // Single instance-method invocation containing the hot loop, so the
    // whole loop body (including the instance-field getfield/putfield
    // idiom, opcode 0xb4) is eligible for OSR compilation as one unit --
    // mirrors JDT Parser.consumeRule's shape (a big instance-method loop
    // dispatching on grammar productions).
    void run(long n) {
        for (long i = 0; i < n; i++) {
            calls++;
            // mirrors JDT's `this.intStack[this.intPtr--]` idiom: getfield
            // (reference-typed array field) + getfield/arith/putfield on an
            // int field + iaload, all in one expression.
            int v = this.intStack[this.intPtr--];
            if (this.intPtr < 0) {
                this.intPtr = 50;
            }
            // allocation pressure between the getfield-produced reference
            // (this.intStack) and further use, to create GC/recompile
            // pressure while an uncommitted reference may be live on the
            // JIT operand stack.
            byte[] junk = new byte[128];
            // reference-element-array arraycopy: always fails the JIT's
            // primitive-element-kind guard and (pre-fix) forced a
            // deopt-and-rerun on every single call.
            System.arraycopy(this.identifierStack, 0, this.identifierStackDst, 0, this.identifierStack.length);
            if (junk.length != 128 || v < 0) {
                mism++;
            }
        }
    }

    public static void main(String[] args) throws Exception {
        long n = args.length > 0 ? Long.parseLong(args[0]) : 200_000L;
        ParserIdiomRepro p = new ParserIdiomRepro();
        p.run(n);
        System.out.println("RESULT calls=" + p.calls + " mism=" + p.mism + " intPtr=" + p.intPtr);
        System.out.println("done");
    }
}
