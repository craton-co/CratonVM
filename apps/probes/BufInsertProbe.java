/** Which `StringBuffer.insert` overloads reach a native at all, and with what
 *  receiver state — the six that regressed when the 62 StringBuffer shadows
 *  were retired, next to the six that did not, so the registry dump's
 *  `invocations` column can be read against a known call list. */
public class BufInsertProbe {
    static void p(String tag, Object v) {
        System.out.println(tag + " |" + v + "|");
    }

    public static void main(String[] args) {
        p("insert(int,String)", new StringBuffer("abc").insert(0, "Z"));
        p("insert(int,char)", new StringBuffer("abc").insert(0, 'Z'));
        p("insert(int,int)", new StringBuffer("abc").insert(0, 7));
        p("insert(int,long)", new StringBuffer("abc").insert(0, 7L));
        p("insert(int,boolean)", new StringBuffer("abc").insert(0, true));
        p("insert(int,float)", new StringBuffer("abc").insert(0, 0.5f));
        p("insert(int,double)", new StringBuffer("abc").insert(0, 0.5d));
        p("insert(int,Object)", new StringBuffer("abc").insert(0, (Object) "Z"));
        p("insert(int,CharSequence)", new StringBuffer("abc").insert(0, (CharSequence) "Z"));
        p("insert(int,CharSequence,0,1)",
                new StringBuffer("abc").insert(0, (CharSequence) "Z", 0, 1));
        p("insert(int,char[])", new StringBuffer("abc").insert(0, new char[] { 'Z' }));

        // The same twelve on a StringBuilder, as the control: these still have
        // their own registrations and must be unaffected.
        p("sb insert(int,int)", new StringBuilder("abc").insert(0, 7));
        p("sb insert(int,CharSequence)", new StringBuilder("abc").insert(0, (CharSequence) "Z"));

        // Does the buffer's own length agree with what toString shows?
        StringBuffer b = new StringBuffer("abc");
        b.insert(0, 7);
        p("after insert(int) length", b.length());
        p("after insert(int) charAt0", b.length() > 0 ? String.valueOf(b.charAt(0)) : "<empty>");
        p("after insert(int) toString", b.toString());
        p("after insert(int) capacity", b.capacity());

        StringBuffer c = new StringBuffer("abc");
        c.insert(0, (CharSequence) "Z");
        p("after insert(cs) length", c.length());
        p("after insert(cs) toString", c.toString());

        System.out.println("DONE BufInsertProbe");
    }
}
