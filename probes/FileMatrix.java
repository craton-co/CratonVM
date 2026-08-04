import java.io.File;
import java.nio.file.*;

public class FileMatrix {
    static void p(String k, Object v) { System.out.println(k + "\t" + v); }

    static void probe(String s) {
        File f = new File(s);
        p("new File(" + s + ").getPath", f.getPath());
        p("new File(" + s + ").getName", f.getName());
        p("new File(" + s + ").getParent", f.getParent());
        p("new File(" + s + ").getAbsolutePath", f.getAbsolutePath());
        p("new File(" + s + ").toPath", f.toPath().toString());
        p("new File(" + s + ").isAbsolute", f.isAbsolute());
        p("new File(" + s + ").toString", f.toString());
    }

    public static void main(String[] a) throws Exception {
        for (String s : new String[]{"/tmp/x/", "/tmp/x", "a/b/", "/", "//", "a//b", "/tmp/"}) probe(s);
        p("File(/tmp,x/).getPath", new File("/tmp", "x/").getPath());
        p("File(dir,name).getPath", new File(new File("/tmp/"), "y/").getPath());
        p("toURI(existing dir)", new File("/tmp/").toURI());
        p("Path.toUri(existing dir)", Paths.get("/tmp/").toUri());
        p("Path.toUri(existing dir no slash)", Paths.get("/tmp").toUri());
        p("Path.toUri(missing)", Paths.get("/tmp/definitely-missing-xyz").toUri());
        p("Path.toFile().getPath", Paths.get("/tmp/x/").toFile().getPath());
        p("File.toPath().equals", new File("/tmp/x/").toPath().equals(Paths.get("/tmp/x")));
    }
}
