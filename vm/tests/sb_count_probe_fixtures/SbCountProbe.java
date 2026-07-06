/**
 * Regression probe for the `sb_set_count` field-slot bug: CratonVM's
 * synthetic StringBuilder/StringBuffer layout is 2 slots (`value: char[]`
 * @0, `count: int` @1 — see `instance_fields(2)` in
 * `classloading/src/class_manager.rs`). A helper introduced to also
 * support the real JDK 9+ 3-slot layout (`value`/`coder`/`count` @0/1/2,
 * relevant only when real `AbstractStringBuilder` bytecode itself
 * constructs the object) unconditionally zeroed slot 1 and wrote the
 * count to slot 2 regardless of which layout the receiver actually has.
 * For the universal 2-slot case this stomped the *only* count-bearing
 * slot to 0 on every append/insert/setLength call and dropped the
 * slot-2 write as out-of-bounds, so `StringBuilder.length()` always
 * read back 0 (or near-0) no matter how much was appended.
 *
 * A Java-level loop keyed on `sb.length()` (e.g.
 * `while (sb.length() < n) sb.append(c);`) never observed the length
 * increase and spun forever, hammering the VM's out-of-bounds field
 * write guard on every iteration.
 */
public class SbCountProbe {
    public static void main(String[] args) {
        // Plain append/length/toString — the minimal case that was
        // completely broken (length() always read back 0).
        StringBuilder sb = new StringBuilder();
        sb.append('a');
        sb.append('b');
        sb.append('c');
        System.out.println("basic_length=" + sb.length());
        System.out.println("basic_toString=" + sb.toString());

        // Force a buffer grow past the default capacity (16) to exercise
        // the count-dependent growth path too.
        StringBuilder grown = new StringBuilder();
        for (int i = 0; i < 40; i++) {
            grown.append((char) ('a' + (i % 26)));
        }
        System.out.println("grown_length=" + grown.length());
        System.out.println("grown_toString=" + grown.toString());

        // The actual failure mode this bug caused: a length-keyed growth
        // loop. Pre-fix, this hung forever (length() never advanced past
        // 0) instead of completing in a handful of iterations.
        StringBuilder padded = new StringBuilder();
        while (padded.length() < 20) {
            padded.append('x');
        }
        System.out.println("padded_length=" + padded.length());
        System.out.println("DONE");
    }
}
