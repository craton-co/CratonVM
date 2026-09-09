# `ServiceLoader.loadInstalled` finds nothing, so every service the JDK looks up through the platform loader is silently empty

**Status:** open, root cause identified 2026-09-09.
**Applies to:** JDK 21 and JDK 25, `--real-jdk` and `--jdk-only` (measured on
Linux; see "scope" below).
**Severity:** wide. This is not a locale defect. It is a `ServiceLoader` defect
that a locale symptom led to, and it silently empties **every** service the JDK
resolves through the platform class loader.
**Found:** narrowing the `21-windows` strict-corpus `textformat` row.

## 1. The measurement

```
                                        load()                        loadInstalled()
HotSpot 21   LocaleDataMetaInfo         2  [CLDR…, NonBase…]           2  [CLDR…, NonBase…]
HotSpot 21   FileSystemProvider         2  [ZipFSP, JrtFSP]            2  [ZipFSP, JrtFSP]
CratonVM 21  LocaleDataMetaInfo         2  [CLDR…, NonBase…]           0  []
CratonVM 21  FileSystemProvider         2  [ZipFSP, JrtFSP]            0  []
```

`ServiceLoader.load(S)` resolves through the **thread-context (application)**
loader and is correct. `ServiceLoader.loadInstalled(S)` resolves through the
**platform** loader and returns **nothing, for every service tried**. Both
loaders exist and print sane identities:

```
platform loader = jdk.internal.loader.ClassLoaders$PlatformClassLoader@520
system   loader = jdk.internal.loader.ClassLoaders$AppClassLoader@24
```

Nothing throws. An empty `ServiceLoader` iteration is indistinguishable from
"this service genuinely has no providers", so every caller silently takes its
no-provider path.

## 2. How it surfaced: every non-English locale answers US data

`DecimalFormatSymbols.getInstance(Locale.GERMANY)` -- asked for **by name**, so
locale *selection* is not the variable -- returns US separators, because
`LocaleProviderAdapter.getAdapter` hands back `FallbackLocaleProviderAdapter`
instead of the CLDR one, and the fallback carries root/English data only.

The chain from the defect to that symptom:

1. `CLDRLocaleProviderAdapter`'s static initialiser looks up the supplementary
   `LocaleDataMetaInfo` with **`ServiceLoader.loadInstalled`**.
2. It gets nothing, so its `nonBaseMetaInfo` is null.
3. Its supported-locale set is then only what `java.base`'s
   `CLDRBaseLocaleDataMetaInfo` provides: **5** tags.
4. `LocaleProviderAdapter.getAdapter(spi, Locale.GERMANY)` asks each adapter
   whether it supports `de-DE`. CLDR truthfully says no.
5. Control falls to `FALLBACK`, whose US data is returned.

```
                    availableLocales   has a `de` locale
  HotSpot 21        1063               true
  CratonVM 21       5                  false
```

and the five are `[]`, `[en]`, `[en_US]`, `[en_US_#Latn]`, `[en_US_POSIX]` --
exactly `java.base`'s own set. Every layer behaves correctly given its inputs.
Only step 1 is wrong.

## 3. Three theories this refutes, all of them mine, all measured

Recording these because each looked right and each cost a probe.

**Not the CLDR adapter failing to load.** It constructs fine under CratonVM,
`LocaleProviderAdapter.forType` returns a real instance for every `Type`, and
`adapterPreference` is `[CLDR, JRE]` -- identical to HotSpot.

**Not `jdk.localedata` missing from the module graph.** This was written up as
the cause on 2026-09-09 and is WRONG. Measured: the module is in the boot layer,
`ModuleFinder.ofSystem()` sees it, both `CLDRLocaleDataMetaInfo` (from
`jdk.localedata`) and `CLDRBaseLocaleDataMetaInfo` (from `java.base`) load by
name from the right modules, and the boot layer has 69 modules.

**Not the locale data itself being unreadable.** Asked directly, both providers
return byte-identical data on CratonVM and HotSpot -- the CLDR provider hands
back **1058** `AvailableLocales` tags on both.

The reason all three probes looked healthy is that every one of them used
`ServiceLoader.load`. The JDK's own call site uses `loadInstalled`. **A probe
that does not use the same lookup the code under test uses cannot see the
defect**, and will keep reporting that everything is fine.

## 4. Scope -- what is measured and what is not

Measured on Linux, JDK 21 and 25, both modes: `loadInstalled` empty in all of
them. Two services tried (`LocaleDataMetaInfo`, `FileSystemProvider`), both
empty, which is why this is stated as a general defect rather than a locale one.
`java.time.chrono.Chronology` is 0 on BOTH VMs and is therefore a control, not
evidence.

**Not established:** that this is the whole story for the Windows `textformat`
row. The 2026-09-08 Windows narrowing recorded `--real-jdk` answering German
data CORRECTLY while only `--jdk-only` was wrong; on Linux both modes fall back.
Candidates, untested: Windows has a real `HostLocaleProviderAdapter` that Linux
does not, and it may claim `de-DE` on the `--real-jdk` arm and mask this;
different builds/images/dates; mode as a second independent variable. Do not
quote this page as "mode-independent" until that is resolved.

Also noticed and NOT chased: under `load()`, CratonVM's `JrtFileSystemProvider`
instantiation reports a `NoSuchMethodError` where HotSpot does not. Unrelated to
`loadInstalled`, possibly its own defect.

## 5. Why this is worth more than the row that found it

`loadInstalled` is how the JDK finds *installed extensions* -- providers that
ship with the platform rather than with the application. Everything resolved
that way is currently invisible to CratonVM, and every one of those call sites
fails the same silent way: no exception, just a service that appears to have no
providers. The locale row is the one symptom that happened to reach a gate.

## 6. Reproducing

`probes/LoadInstalled.java` is the direct test and takes one run:

```
cratonvm --add-exports java.base/sun.util.locale.provider=ALL-UNNAMED -cp . LoadInstalled
#   load()          = 2
#   loadInstalled() = 0     <- the defect
```

`probes/LocaleAdapter.java` (the adapter chain), `probes/CldrLocales.java` (the
5-vs-1063 count), `probes/CldrWhich.java` (which five), `probes/LocaleModule.java`
(the module graph, which is healthy) and `probes/LocaleTags.java` (the provider
data, which is healthy) are the narrowing, kept because three of them are the
refutations in section 3.

Keep `en-US` as a control in any locale probe: it reads the same on a healthy
and a broken VM, so a run where the control differs is a broken probe.

## 7. Where to look

`ServiceLoader.loadInstalled(S)` is `ServiceLoader.load(S,
ClassLoader.getPlatformClassLoader())`. So the question is why provider lookup
against the PLATFORM loader yields nothing while the same lookup against the
application loader yields the right providers -- module-provided services are
found by walking the resolved modules readable by that loader, so start with
what CratonVM's platform loader reports for that walk, not with `ServiceLoader`
itself.

Related: `docs/known-issues/jdk-only/the-first-jdk-21-run-found-the-javalangaccess-carrier-is-pinned-to-system-1-20260908.md`
finding #4, which this completes.
