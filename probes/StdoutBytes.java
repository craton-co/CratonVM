// Companion measurement for the encoding-fidelity residual of
// `bug-printstream-charset-answers-the-abstract-base-20260825-FIXED-20260901.md` §5.
//
// The system properties are only half the claim; what matters is the BYTES
// `System.out.println` actually puts on fd 1. Under a C/POSIX locale HotSpot
// encodes stdout as US-ASCII and substitutes `?` for anything it cannot map;
// a VM that reports `stdout.encoding=UTF-8` but emits UTF-8 bytes is
// self-consistent but differs from HotSpot, and a VM that reports the locale
// encoding while still emitting UTF-8 would be worse than either.
//
//   javac -d probes/out probes/StdoutBytes.java
//   java -cp probes/out StdoutBytes | od -An -tx1
public class StdoutBytes {
    public static void main(String[] a) {
        System.out.print("[");
        System.out.print("Ж");     // CYRILLIC CAPITAL ZHE
        System.out.print("é");     // e-acute (in Cp1251? no; in ISO-8859-1 yes)
        System.out.print("]");
        System.out.flush();
        System.err.print("[Ж]");
        System.err.flush();
    }
}
