# `Binder` drops the nested children of a key that is also a scalar value

**Status: OPEN — found 2026-07-31**

## Symptom

`GitInfoContributorTests.withGitIdAndAbbrev` (gh-11892 regression test) fails:

```
=> java.lang.AssertionError:
Expecting actual:
  "1b3cec34f7ca0a021244452f2cae07a80497a7c7"
to be an instance of:
  java.util.Map
but was instance of:
  java.lang.String
       org.springframework.boot.actuate.info.GitInfoContributorTests.withGitIdAndAbbrev(GitInfoContributorTests.java:82)
```

## Root cause (not yet fully pinned down)

The test builds a `Properties` with three keys — `branch`, `commit.id`, and
`commit.id.abbrev` — where `commit.id` is simultaneously a leaf scalar value
*and* a prefix of the deeper key `commit.id.abbrev`
(`.../info/GitInfoContributorTests.java:74-86`). `GitInfoContributor` (mode
`FULL`) exposes this via
`InfoPropertiesInfoContributor.extractContent`
(`.../info/InfoPropertiesInfoContributor.java:98-101`):

```java
Iterable<ConfigurationPropertySource> adapted = ConfigurationPropertySources.from(propertySource);
new Binder(adapted).bind("", Bindable.mapOf(String.class, Object.class))
```

On real Spring Boot, this ambiguous-key case is a well-known, deliberately
supported shape: `Binder`'s `Map<String,Object>` binding detects that `id`
has both a direct value and descendants, and produces a nested map
`{"full": "<the scalar>", "abbrev": "<commit.id.abbrev's value>"}` for `id`
rather than dropping either side. The `shortenCommitId` test in the same file
(a plain `commit.id` with *no* `commit.id.abbrev` sibling) passes, so the
simple scalar-only case binds fine.

On CratonVM, `commit.get("id")` comes back as the bare scalar string — i.e.
the `commit.id.abbrev` key's contribution is lost entirely, and the "value
also has children" ambiguity handling that would have promoted `id` to a
nested map never kicks in. `Binder`/`MapBinder` is generic Spring
infrastructure exercised very widely elsewhere in the suite without this
symptom, so the defect looks narrow: it reproduces specifically when a
`ConfigurationPropertySource`-backed `Properties` map contains a key that is
simultaneously a leaf value and an ancestor of another key. Candidates worth
checking in a follow-up: `Properties`/`Hashtable` key enumeration order
(`stringPropertyNames()` on CratonVM's `java.util.Properties`) silently
dropping `commit.id.abbrev`, or `ConfigurationPropertyName` ancestor/descendant
comparison logic inside `Binder`/`MapBinder` misclassifying the ambiguous
key. Not root-caused within this pass — needs a smaller repro directly
exercising `Binder.bind` (or even just `Properties.stringPropertyNames()`)
against this three-key shape.

## Affected classes

- `module/spring-boot-actuator` — `org.springframework.boot.actuate.info.GitInfoContributorTests`
