import java.io.*; import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;
public class LuceneMini {
  static long rss() {
    try (BufferedReader r = new BufferedReader(new FileReader("/proc/self/status"))) {
      for (String l; (l = r.readLine()) != null; ) if (l.startsWith("VmRSS:")) return Long.parseLong(l.replaceAll("[^0-9]",""))/1024;
    } catch (Exception e) {}
    return -1;
  }
  static void mark(String s) { System.out.println("rss=" + rss() + "MB  " + s); System.out.flush(); }
  public static void main(String[] a) throws Exception {
    mark("start");
    Path p = Path.of("luceneidx");
    Directory dir = FSDirectory.open(p);
    mark("FSDirectory.open -> " + dir.getClass().getSimpleName());
    StandardAnalyzer an = new StandardAnalyzer();
    mark("StandardAnalyzer");
    IndexWriterConfig cfg = new IndexWriterConfig(an);
    mark("IndexWriterConfig");
    IndexWriter w = new IndexWriter(dir, cfg);
    mark("new IndexWriter");
    Document d = new Document();
    d.add(new TextField("f", "hello world", Field.Store.YES));
    w.addDocument(d);
    mark("addDocument");
    w.commit();
    mark("commit");
    w.close();
    mark("close");
    DirectoryReader rd = DirectoryReader.open(dir);
    mark("DirectoryReader.open numDocs=" + rd.numDocs());
    rd.close(); dir.close();
    mark("DONE");
  }
}
