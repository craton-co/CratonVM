import java.io.File;

/** java.io.File's PATH-STRING surface, swept against whatever VM runs it.
 *
 *  These methods are pure functions of the path string, so this is a two-VM
 *  stdout diff with no fixture and no I/O. Deliberately EXCLUDES exists() /
 *  length() / lastModified() and friends: those depend on the disk, which is
 *  not identical between two runs and would put noise in the diff.
 *
 *  Backslashes matter here (Windows is the host), so this file is written
 *  directly rather than through a shell heredoc, which eats one level of them.
 */
public class FilePathSweep {
    static final String[] PATHS = {
        "", ".", "..", "/", "//", "///",
        "\\", "\\\\",
        "a", "a/b", "a/b/c", "/a", "/a/b", "a/", "a//b", "a/./b", "a/../b",
        "C:", "C:/", "C:\\", "C:/x", "C:\\x\\y",
        "\\\\server\\share", "\\\\server\\share\\f", "//server/share",
        "relative\\win\\path",
        "trailing/", "trailing\\", "  spaced  ", "with space/and more",
        "unicode/\u00e9\u4e2d\u6587", "dot.ext", ".hidden", "a.b.c.d",
        "/very/deep/path/that/keeps/going/on/and/on", "x/y/../../z",
        "..\\..\\up", "/..", "/.",
    };

    /** Escape every non-ASCII char as a backslash-u-XXXX hex escape.
     *
     *  Without this the diff is a function of each VM's stdout ENCODING, not of
     *  its behaviour: HotSpot on this host reports stdout.encoding=Cp1251 and
     *  writes `?` for the CJK path, while CratonVM reports UTF-8 and writes the
     *  real bytes. That produced 7 differing lines whose VALUES were identical. */
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    public static void main(String[] args) {
        p("separator", File.separator);
        p("pathSeparator", File.pathSeparator);
        for (String s : PATHS) {
            File f;
            try { f = new File(s); }
            catch (Throwable t) { p("CTOR-THREW " + s, t.getClass().getName()); continue; }
            String k = "[" + s + "]";
            try { p(k + " getName", f.getName()); }             catch (Throwable t) { p(k + " getName THREW", t.getClass().getName()); }
            try { p(k + " getParent", f.getParent()); }         catch (Throwable t) { p(k + " getParent THREW", t.getClass().getName()); }
            try { p(k + " getPath", f.getPath()); }             catch (Throwable t) { p(k + " getPath THREW", t.getClass().getName()); }
            try { p(k + " isAbsolute", f.isAbsolute()); }       catch (Throwable t) { p(k + " isAbsolute THREW", t.getClass().getName()); }
            try { p(k + " toString", f.toString()); }           catch (Throwable t) { p(k + " toString THREW", t.getClass().getName()); }
            try { p(k + " hashCode", f.hashCode()); }           catch (Throwable t) { p(k + " hashCode THREW", t.getClass().getName()); }
            try { p(k + " getParentFile", String.valueOf(f.getParentFile())); } catch (Throwable t) { p(k + " getParentFile THREW", t.getClass().getName()); }
            try { p(k + " toURI", f.toURI()); }                 catch (Throwable t) { p(k + " toURI THREW", t.getClass().getName()); }
            try { p(k + " absPathNonEmpty", f.getAbsolutePath().length() > 0); } catch (Throwable t) { p(k + " getAbsolutePath THREW", t.getClass().getName()); }
            for (String o : new String[]{"a", "a/b", s}) {
                try { p(k + " compareTo(" + o + ")", Integer.signum(f.compareTo(new File(o)))); }
                catch (Throwable t) { p(k + " compareTo THREW", t.getClass().getName()); }
                try { p(k + " equals(" + o + ")", f.equals(new File(o))); }
                catch (Throwable t) { p(k + " equals THREW", t.getClass().getName()); }
            }
            try { p(k + " child", new File(f, "kid").getPath()); }  catch (Throwable t) { p(k + " child THREW", t.getClass().getName()); }
            try { p(k + " twoArg", new File(s, "kid").getPath()); } catch (Throwable t) { p(k + " twoArg THREW", t.getClass().getName()); }
        }
        System.out.println("DONE FilePathSweep");
    }
}
