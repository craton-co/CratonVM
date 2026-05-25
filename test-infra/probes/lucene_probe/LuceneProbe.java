import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.Document;
import org.apache.lucene.document.Field;
import org.apache.lucene.document.StringField;
import org.apache.lucene.document.TextField;
import org.apache.lucene.index.DirectoryReader;
import org.apache.lucene.index.IndexWriter;
import org.apache.lucene.index.IndexWriterConfig;
import org.apache.lucene.index.Term;
import org.apache.lucene.queryparser.classic.QueryParser;
import org.apache.lucene.search.IndexSearcher;
import org.apache.lucene.search.Query;
import org.apache.lucene.search.TopDocs;
import org.apache.lucene.store.ByteBuffersDirectory;
import org.apache.lucene.store.Directory;
public class LuceneProbe {
    public static void main(String[] args) throws Exception {
        Directory dir = new ByteBuffersDirectory();
        StandardAnalyzer analyzer = new StandardAnalyzer();
        IndexWriterConfig cfg = new IndexWriterConfig(analyzer);
        try (IndexWriter w = new IndexWriter(dir, cfg)) {
            for (String text : new String[]{"hello world", "lucene full text search", "cratonvm jvm probe"}) {
                Document d = new Document();
                d.add(new StringField("id", text, Field.Store.YES));
                d.add(new TextField("body", text, Field.Store.YES));
                w.addDocument(d);
            }
        }
        try (DirectoryReader r = DirectoryReader.open(dir)) {
            IndexSearcher s = new IndexSearcher(r);
            QueryParser p = new QueryParser("body", analyzer);
            Query q = p.parse("cratonvm");
            TopDocs hits = s.search(q, 10);
            System.out.println("Hits: " + hits.totalHits);
            if (hits.totalHits.value < 1) { System.out.println("FAIL"); System.exit(1); }
            System.out.println("Top: " + s.storedFields().document(hits.scoreDocs[0].doc).get("body"));
        }
        System.out.println("OK");
        System.exit(0);
    }
}
