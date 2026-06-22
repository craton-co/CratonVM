# SC-stax-xml-family — Synthetic StAX → SAX bridge diverges (missing cursor natives + dropped element prefix)

## Title
StAX `XMLStreamReader`/`XMLEventReader` → SAX bridge produces wrong output: three unregistered cursor natives (`getTextCharacters`, `getAttributePrefix`, plus latent `getTextStart`/`getTextLength`) abort the Stream path with `AbstractMethodError`, and `getName()` hardcodes an empty element prefix, dropping `prefix:` from SAX qNames on both paths.

## Symptom
Two failure modes, split cleanly by path:

1. **Stream path (`StaxStreamXMLReaderTests`, 5 fails)** — `java.lang.AbstractMethodError`:
   - `method javax/xml/stream/XMLStreamReader.getAttributePrefix(I)Ljava/lang/String; has no Code attribute`
   - `method javax/xml/stream/XMLStreamReader.getTextCharacters()[C has no Code attribute`

   The interface method exists (declared abstract on `javax.xml.stream.XMLStreamReader`) but no CratonVM native is registered for it, so dispatch finds an abstract method with no body → `AbstractMethodError`.

2. **Event path (`StaxEventXMLReaderTests`, 2 fails)** — `java.lang.AssertionError` (empty message). The document parses, but the SAX call sequence emitted by the bridge differs from the JDK reference SAX parser, so Spring's `MockitoUtils.verifySameInvocations(standardContentHandler, contentHandler)` fails.

## Affected tests
- `org.springframework.util.xml.StaxStreamXMLReaderTests` (5 fails):
  - `contentHandlerNamespacesPrefixes()` → AbstractMethodError `getAttributePrefix(I)`
  - `contentHandlerNamespacesNoPrefixes()` → AbstractMethodError `getAttributePrefix(I)`
  - `whitespace()` → AbstractMethodError `getTextCharacters()[C`
  - `contentHandlerNoNamespacesPrefixes()` → AbstractMethodError `getTextCharacters()[C`
  - `lexicalHandler()` → AbstractMethodError `getTextCharacters()[C`
  - (`partial()` is the lone pass — see Root cause #2.)
- `org.springframework.util.xml.StaxEventXMLReaderTests` (2 fails):
  - `contentHandlerNamespacesPrefixes()` → AssertionError
  - `contentHandlerNamespacesNoPrefixes()` → AssertionError

All these tests pass on HotSpot/JDK25; each failure is a CratonVM divergence.

---

## Root cause(s)

### Root cause #1 — `getTextCharacters()`, `getAttributePrefix(int)` (and latent `getTextStart()`/`getTextLength()`) are not registered as natives (Stream path)

CratonVM's synthetic StAX impl lives in `native-builtins/src/xml_stax.rs`. The native registration block is `register()` at **`native-builtins/src/xml_stax.rs:1387`**, with the `XMLStreamReader` cursor methods registered from **line 1505 through 1819**. That block registers `hasNext, next, getEventType, getLocalName, getText, getNamespaceURI, getAttributeValue, getAttributeLocalName, getAttributeCount, close, isStartElement, isEndElement, isCharacters, isWhiteSpace, require, nextTag, getElementText, getName, getAttributeName, getAttributeNamespace, hasName, hasText, getPrefix, getNamespaceContext, getNamespaceCount, getNamespaceURI(i), getNamespacePrefix(i), getProperty, getAttributeType, isAttributeSpecified, getPITarget, getPIData, isStandalone, standaloneSet, getCharacterEncodingScheme, getEncoding, getVersion, getLocation` — but **not** `getTextCharacters`, `getAttributePrefix`, `getTextStart`, or `getTextLength`. (Confirmed: a grep for all four names across `xml_stax.rs` returns zero hits.)

The Spring bridge `StaxStreamXMLReader` calls the missing methods directly:
- `getAttributePrefix(i)` — `StaxStreamXMLReader.java:175` (in `handleStartElement`, inside the `hasNamespacesFeature()` branch).
- `getTextCharacters()` — `StaxStreamXMLReader.java:214` (`handleCharacters`) and `:224` (`handleComment`).
- `getTextStart()` / `getTextLength()` — `StaxStreamXMLReader.java:215, 216, 225` — these are **also unregistered** and become the *next* `AbstractMethodError` the moment `getTextCharacters` is added. They must be fixed in the same change or the Stream tests will simply fail one method later.

This maps the five Stream failures exactly:
- namespaces-on tests (`contentHandlerNamespacesPrefixes`, `contentHandlerNamespacesNoPrefixes`) enter the `hasNamespacesFeature()` branch and hit `getAttributePrefix(i)` first.
- `contentHandlerNoNamespacesPrefixes` (namespaces off) skips that branch and instead reaches the character data (`" Some text "` in the fixture) → `getTextCharacters()`.
- `whitespace` and `lexicalHandler` reach character/comment data → `getTextCharacters()`.

All the data needed already exists in the in-memory event model — no parser change required:
- `StaxAttr` carries a `prefix` field (`xml_stax.rs:116`), so `getAttributePrefix(i)` is `e.attributes[i].prefix` (mirror of the existing `native_get_attr_namespace` at `xml_stax.rs:1944`).
- `StaxEvent.text` (`xml_stax.rs`, struct at `:75`) holds the character/comment/CDATA payload; `getTextStart()=0`, `getTextLength()=text length (in UTF-16 code units)`, and `getTextCharacters()` is `text` as a `char[]`. The `NativeContext` API already exposes `new_array(ArrayElementType::Char, len)` + `write_char_array_from(...)` (`native-api/src/registry.rs:547, 702`) and `create_string` (`:753`), so building the `char[]` is mechanical.

### Root cause #2 — `getName()` returns a `QName` with an empty `prefix`, dropping `prefix:` from SAX qNames (Event path now; Stream path after #1 is fixed)

`native_get_qname` (the impl behind `XMLStreamReader.getName()`) hardcodes the QName prefix to `""` at **`native-builtins/src/xml_stax.rs:1903`**:

```rust
let prefix_s = ctx.create_string("");   // <-- always empty for elements
ctx.set_field_by_name(qname, "localPart", ...);
ctx.set_field_by_name(qname, "namespaceURI", ...);
ctx.set_field_by_name(qname, "prefix", Value::Object(Some(prefix_s)));
```

The real element prefix is parsed and stored — `StaxEvent.prefix` (`xml_stax.rs:86`), populated for START/END element events in `make_element_event` (`xml_stax.rs:475`) and the End/Empty branches (`:359, :368`). The sibling `native_get_attr_qname` correctly carries `a.prefix.clone()` (`xml_stax.rs:1937`); `native_get_qname` simply fails to read `e.prefix` and substitutes `""`.

Why this breaks the Event tests: the Event path wraps the synthetic cursor in the **real JDK** `com.sun.xml.internal.stream.XMLEventReaderImpl` (`xml_stax.rs:782-789`), whose `XMLEventAllocatorImpl` builds each `StartElement`/`EndElement` event by calling our `getName()`. The bridge `StaxEventXMLReader.handleStartElement`/`handleEndElement` then computes the SAX qualified name via `AbstractStaxXMLReader.toQualifiedName(qName)` (`AbstractStaxXMLReader.java:120-128`), which returns `prefix + ":" + localPart` when a prefix is present, else just `localPart`. With prefix forced to `""`, CratonVM emits SAX `startElement("...","hello","hello",...)` where the JDK reference emits `startElement("...","hello","h:hello",...)`.

The test fixture `testContentHandler.xml` is entirely prefixed:
```
<h:hello xmlns:h="..." id="a1" h:person="David"><prefix:goodbye xmlns:prefix="..." h:person="Arjen"> Some text </prefix:goodbye><h:so-long> </h:so-long></h:hello>
```
so every element (`h:hello`, `prefix:goodbye`, `h:so-long`) produces a qName mismatch against the reference → `verifySameInvocations` → `AssertionError`. Both failing Event tests have the namespaces feature ON, which is the branch that routes through `toQualifiedName(qName)` (`StaxEventXMLReader.java:200, 230`).

**Note the shared blast radius:** `native_get_qname` backs `getName()` on the Stream path too (`StaxStreamXMLReader.java:169, 192`). The Stream namespace tests die earlier on Root cause #1, so this prefix bug is currently masked there — but once #1 is fixed, the Stream `contentHandlerNamespaces*` tests will hit the *same* dropped-prefix mismatch. Both root causes must be fixed for the Stream namespace tests to pass.

**Why `partial()` (Stream) passes:** it asserts `streamReader.getName()).isEqualTo(new QName(ns, "root"))` — `QName.equals` compares only namespaceURI + localPart and ignores prefix (per the JAXP `QName` contract), so the empty prefix is invisible to that assertion. It also never reaches `getTextCharacters`/`getAttributePrefix`.

---

## Reproduction sketch

Minimal Java (covers both root causes):
```java
import java.io.StringReader;
import javax.xml.stream.*;

public class StaxRepro {
  public static void main(String[] a) throws Exception {
    String xml = "<h:hello xmlns:h='urn:x' h:k='v'> text </h:hello>";
    XMLStreamReader r = XMLInputFactory.newInstance()
        .createXMLStreamReader(new StringReader(xml));
    r.next();                                  // -> START_ELEMENT
    System.out.println("prefix=[" + r.getName().getPrefix() + "]");   // JDK: "h"   CratonVM: ""   (RC#2)
    System.out.println("attrPrefix=[" + r.getAttributePrefix(0) + "]"); // CratonVM: AbstractMethodError (RC#1)
    r.next();                                  // -> CHARACTERS
    char[] c = r.getTextCharacters();          // CratonVM: AbstractMethodError (RC#1)
    System.out.println("text=[" + new String(c, r.getTextStart(), r.getTextLength()) + "]");
  }
}
```

cratonvm command (do NOT run while the suite is active):
```
cratonvm --java-home <jdk25> -cp <classpath> StaxRepro
```
Expected on HotSpot: `prefix=[h]`, `attrPrefix=[h]`, `text=[ text ]`. On CratonVM: `prefix=[]` then `AbstractMethodError` on `getAttributePrefix`.

Or directly run the Spring tests:
`StaxStreamXMLReaderTests` (5/6 fail) and `StaxEventXMLReaderTests` (2/6 fail) in `spring-core`.

## Suspected subsystem
`native-builtins` — synthetic StAX implementation `native-builtins/src/xml_stax.rs` (native registry coverage + element-QName construction). No GC/JIT/classloading involvement.

## Severity
**Medium.** Two clearly-bounded, data-already-present bugs in a self-contained synthetic subsystem. Affects any Spring/JAXP StAX→SAX bridging of namespaced XML (e.g. Spring-WS, OXM). Not a correctness hazard for unrelated code, but the dropped element prefix silently corrupts SAX qNames for any prefixed document, which can propagate wrong namespace-prefix mappings downstream.

## Confidence
**High.** Both root causes are pinned to exact lines with the data model and call sites cross-checked end-to-end:
- RC#1: the four methods are provably absent from the registry; the bridge call sites at `StaxStreamXMLReader.java:175/214/224/215/216/225` map 1:1 to the five Stream failures and the AbstractMethodError messages in the log.
- RC#2: `xml_stax.rs:1903` hardcodes `""`; `e.prefix` is populated; `toQualifiedName` (`AbstractStaxXMLReader.java:120`) consumes the prefix; the all-prefixed fixture explains both Event AssertionErrors and the `partial()` pass.

## Recommendation
**Fix.** Small, low-risk, fully static-data change in one file (`xml_stax.rs`):
1. Register `getAttributePrefix(I)Ljava/lang/String;` → return `e.attributes[i].prefix` (clone `native_get_attr_namespace`, `xml_stax.rs:1944`).
2. Register `getTextCharacters()[C`, `getTextStart()I`, `getTextLength()I` from `StaxEvent.text` (build the `char[]` via `new_array(ArrayElementType::Char, n)` + `write_char_array_from`; start=0, length=UTF-16 length). Add all three together so the Stream path does not just fail one method later.
3. In `native_get_qname` (`xml_stax.rs:1892`), read `e.prefix` and pass it to the QName's `prefix` field instead of the literal `""` (mirror `native_get_attr_qname` at `:1937`).

One-line reason: all required data already lives in `StaxEvent`/`StaxAttr`; this is registry coverage + one dropped field, not a design gap.

## Open questions
- After fixing, do the Stream `contentHandlerNamespaces*` tests pass fully, or is there a further divergence in `startPrefixMapping`/`endPrefixMapping` ordering or attribute reporting? (`getAttributePrefix` was the *first* missing method; verify no additional Stream-only divergence surfaces once it returns real data.)
- `getTextStart()` semantics: the JDK may return a non-zero offset into a shared backing buffer. CratonVM stores each event's text independently, so `getTextStart()=0` / `getTextLength()=text.len()` with `getTextCharacters()` returning exactly that slice is internally consistent — confirm no caller relies on a shared-buffer offset convention (Spring's bridge uses the returned triple consistently, so this should be safe).
- The Event path's `getNamespaceContext()` returns a *fresh empty* `NamespaceContextWrapper` (`xml_stax.rs:1141`). The two failing Event tests are explained by RC#2 alone, but if any residual Event AssertionError remains after the prefix fix, the empty namespace context (which does not reflect in-scope `xmlns` bindings) is the next thing to check for `startPrefixMapping` divergence.
- `getTextLength()` should be measured in UTF-16 code units to match `char[]` length for non-BMP text; confirm the impl counts code units, not Rust `String` bytes/chars.
