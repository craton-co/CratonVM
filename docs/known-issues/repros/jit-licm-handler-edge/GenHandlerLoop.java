import org.objectweb.asm.*;
import java.nio.file.*;

/**
 * Emits HandlerLoopProbe.class — a shape javac cannot produce from source: an
 * exception handler whose PC lies INSIDE a loop body while its protected range
 * lies entirely BEFORE the loop header. Entering that handler delivers control
 * into the loop without the JIT's speculative pre-header (the LICM hoist of
 * `n * 5`) having run.
 *
 * Critically, the NORMAL entry falls through into the loop header — there is no
 * `goto` into it — so the exception edge is the ONLY entry the backend's
 * bytecode branch decoding cannot see. That isolates the exception-handler hole
 * from the already-fixed explicit-branch case.
 *
 *   static int shape(int base, int n, int trip)
 *     try {                             // protected range: entirely pre-loop
 *         if (trip != 0) throw new ArithmeticException();
 *         max = 25;
 *     }                                 // falls through into LOOP_HEAD
 *   LOOP_HEAD:
 *     while (max < n * 5) { max *= 2; } // n * 5 is the hoist candidate
 *     return max;
 *   handler:                            // INSIDE the loop body
 *     pop; max = 25; goto LOOP_HEAD;
 *
 * Both entries set max = 25 and must return the same value.
 */
public class GenHandlerLoop {
    public static void main(String[] args) throws Exception {
        ClassWriter cw = new ClassWriter(ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS);
        cw.visit(Opcodes.V1_8, Opcodes.ACC_PUBLIC | Opcodes.ACC_SUPER,
                "HandlerLoopProbe", null, "java/lang/Object", null);

        MethodVisitor ctor = cw.visitMethod(Opcodes.ACC_PUBLIC, "<init>", "()V", null, null);
        ctor.visitCode();
        ctor.visitVarInsn(Opcodes.ALOAD, 0);
        ctor.visitMethodInsn(Opcodes.INVOKESPECIAL, "java/lang/Object", "<init>", "()V", false);
        ctor.visitInsn(Opcodes.RETURN);
        ctor.visitMaxs(0, 0);
        ctor.visitEnd();

        // static int shape(int base, int n, int trip)   locals: 0=base 1=n 2=trip 3=max
        MethodVisitor mv = cw.visitMethod(Opcodes.ACC_PUBLIC | Opcodes.ACC_STATIC,
                "shape", "(III)I", null, null);
        Label tryStart = new Label();
        Label tryEnd = new Label();
        Label handler = new Label();
        Label loopHead = new Label();
        Label loopBody = new Label();
        Label done = new Label();
        Label noThrow = new Label();

        mv.visitTryCatchBlock(tryStart, tryEnd, handler, "java/lang/ArithmeticException");
        mv.visitCode();

        // IMPLICIT throw only: an explicit ATHROW disqualifies the method from
        // JIT compilation entirely (has_athrow admission gate), so use a
        // division by zero. trip == 0 -> ArithmeticException -> handler entry;
        // trip != 0 -> normal fall-through entry.
        mv.visitLabel(tryStart);
        mv.visitInsn(Opcodes.ICONST_1);
        mv.visitVarInsn(Opcodes.ILOAD, 2); // trip
        mv.visitInsn(Opcodes.IDIV); // throws when trip == 0
        mv.visitInsn(Opcodes.POP);
        mv.visitLabel(noThrow);
        mv.visitIntInsn(Opcodes.BIPUSH, 25);
        mv.visitVarInsn(Opcodes.ISTORE, 3); // max = 25
        mv.visitLabel(tryEnd); // protected range ends here, BEFORE the loop
        // no goto: fall straight through into the loop header

        // ---- loop: entered by fall-through only ----
        mv.visitLabel(loopHead);
        mv.visitVarInsn(Opcodes.ILOAD, 3); // max
        mv.visitVarInsn(Opcodes.ILOAD, 1); // n
        mv.visitInsn(Opcodes.ICONST_5);
        mv.visitInsn(Opcodes.IMUL); // n * 5  <-- loop-invariant hoist candidate
        mv.visitJumpInsn(Opcodes.IF_ICMPGE, done);
        mv.visitJumpInsn(Opcodes.GOTO, loopBody);

        // Handler sits INSIDE the loop body. Its only predecessor is the
        // exception edge from the pre-loop protected range — invisible to the
        // backend's branch decoding.
        mv.visitLabel(handler);
        mv.visitInsn(Opcodes.POP); // discard the exception
        mv.visitIntInsn(Opcodes.BIPUSH, 25);
        mv.visitVarInsn(Opcodes.ISTORE, 3); // max = 25, same as the normal path
        mv.visitJumpInsn(Opcodes.GOTO, loopHead);

        mv.visitLabel(loopBody);
        mv.visitVarInsn(Opcodes.ILOAD, 3);
        mv.visitInsn(Opcodes.ICONST_2);
        mv.visitInsn(Opcodes.IMUL);
        mv.visitVarInsn(Opcodes.ISTORE, 3); // max *= 2
        mv.visitJumpInsn(Opcodes.GOTO, loopHead); // back edge

        mv.visitLabel(done);
        mv.visitVarInsn(Opcodes.ILOAD, 3);
        mv.visitInsn(Opcodes.IRETURN);
        mv.visitMaxs(0, 0);
        mv.visitEnd();

        cw.visitEnd();
        Files.write(Paths.get("classes/HandlerLoopProbe.class"), cw.toByteArray());
        System.out.println("wrote classes/HandlerLoopProbe.class");
    }
}
