# Porting epubkit to Rust

This repository has been converted from the original Python/FastAPI web app
into a native desktop application: a Rust core, a thin CLI for validation, and
a Tauri front-end reusing the original HTML and CSS.

The Python implementation has been removed now that the port is complete, along
with the `static/` and `templates/` assets its web app served. All of it
survives in git history at `7cf9a65`, which is the pinned reference for any
comparison — see "Validating against the reference" below. The desktop
front-end carries its own descendants of that HTML and CSS in
`crates/desktop/ui/`.

## Layout

```
crates/core/     epubkit-core    — the pipeline, as a library
crates/cli/      epubkit-cli     — `epubkit`, a thin driver for testing the core
crates/desktop/  epubkit-desktop — the Tauri window
```

Module names mirror the Python ones so the two can be read side by side:

| Python                | Rust                  | Status |
|-----------------------|-----------------------|--------|
| `epub_packager.py`    | `core::package`       | ported |
| `metadata_handler.py` | `core::metadata`      | ported |
| `epub_structure.py`   | `core::structure`     | ported |
| `html_cleaner.py`     | `core::html`, `core::css` | ported |
| `text_cleaner.py`     | `core::text`          | ported |
| —                     | `core::xml`           | new: shared libxml2 wrapper |
| `image_processor.py`  | `core::image`         | ported; cover generation deliberately omitted |
| `epub_processor.py`   | `core::pipeline`      | ported |
| —                     | `core::settings`      | new: persisted options and presets |

## Build prerequisites

The XHTML repair step links against libxml2 — the same C library lxml wraps,
chosen so the port's parse/serialize behaviour stays comparable to the
reference implementation.

```sh
# Debian/Ubuntu
apt-get install libxml2-dev

# macOS
brew install pkgconf libxml2
```

The `libxml` crate finds libxml2 through pkg-config and has **no option to
build it from source**, so the host must provide both the library and
pkg-config itself.

macOS needs `pkgconf` explicitly — it is not installed by default, and without
it the build fails with "The pkg-config command could not be found". It also
needs a findable libxml2: the SDK ships one, but its `.pc` file is not on
pkg-config's default search path, and Homebrew's is keg-only. `.cargo/config.toml`
in this repo points `PKG_CONFIG_PATH` at both Homebrew prefixes so neither has
to be set by hand; a value you export yourself still takes precedence.

Windows needs libxml2 via vcpkg (the crate's build script looks there). For
packaging, vendoring and statically linking is likely the better answer than
depending on a system copy — which would mean replacing the `libxml` crate,
since it cannot vendor.

The desktop crate additionally needs a webview and GTK:

```sh
# Debian/Ubuntu
apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev patchelf
```

macOS and Windows use the system webview and need nothing extra.

## Running the window

```sh
cargo run -p epubkit-desktop
```

The front-end is plain HTML, CSS and one ES module under
`crates/desktop/ui/` — no build step, no bundler, no framework. The stylesheet
is the original web app's, carried across nearly unchanged.

The page holds no processing logic and no notion of what a preset means: it
renders whatever `settings` the core hands back and asks the core to change it,
so the window and the CLI cannot drift apart. Commands are ordinary functions,
so the IPC layer is covered by tests in `crates/desktop/tests/` rather than
resting on a screenshot.

Two things about that boundary are easy to get wrong silently, and both are now
pinned by tests:

- `app.withGlobalTauri` must stay `true` in `tauri.conf.json`. The front-end has
  no bundler, so it reaches the API through `window.__TAURI__`; without the flag
  that object does not exist and the module throws on its first line. The window
  still opens and the static HTML still renders, so the failure looks like
  nothing happening rather than like an error.
- `OptionSet` serializes snake_case, which keeps `settings.toml` hand-editable.
  The page must bind `data-option` attributes to those same names.
  `the_page_binds_to_option_keys_that_exist` reads the real `index.html` and
  checks every binding against the real serialization, in both directions — so
  an option added to one and not the other fails the build rather than quietly
  doing nothing. The filename buttons and the template fields the page lists
  are checked against the core the same way.

## Using the CLI

```sh
cargo run -p epubkit-cli -- info      book.epub
cargo run -p epubkit-cli -- validate  book.epub
cargo run -p epubkit-cli -- roundtrip book.epub -o out.epub
cargo run -p epubkit-cli -- repair    chapter.xhtml
cargo run -p epubkit-cli -- optimize  book.epub
cargo run -p epubkit-cli -- optimize  book.epub --filename-template '{series} {series_index} - {title}'
cargo run -p epubkit-cli -- settings  show
```

## Settings

`core::settings` persists what the user last chose, plus any presets they
saved, to `settings.toml` in the platform's config directory.

The model is one live set of option values plus a pointer to which preset the
UI should show as selected. **The values are the truth; the pointer is a
label.** On launch the values are restored verbatim — whether the user last had
a built-in preset, a saved one, or something they tweaked by hand — so "restore
what I had" is one rule rather than three cases. Restoring by value also means
redefining a built-in preset in a later version cannot silently rewrite
someone's stored choices.

Selecting a preset copies its values in. Changing any option moves the
selection to Custom, matching how the web UI already behaves; the difference is
that Custom now persists and can be given a name.

The device is deliberately *not* part of a preset — it describes the hardware
on the desk, not a processing taste — so it is sticky on its own and survives
every preset change. How finished books are named is kept apart the same way:
it is about the user's library, not about any one way of processing.

On the CLI, saved settings are the base and the flags are overrides: each
`--no-*` flag can only turn something off, so an option nobody mentioned keeps
whatever it had. `--filename` and `--filename-template` are remembered too.
Metadata edits (`--title`, `--author`) are about one book and are never
persisted.

## Validating against the reference

The Python implementation at commit `7cf9a65` is the oracle. Pin that SHA
rather than tracking a branch, so the baseline cannot shift mid-port:

```sh
git show 7cf9a65:epub_processor.py
```

Because the history holds every Python file at that commit, the reference
survives even after the working tree drops it — and even if the upstream
repository (`b1rdmania/epubkit`, which this was forked from) disappears.

## Known divergences from the Python

These are deliberate. A diff harness comparing the two implementations should
expect them rather than flag them.

### XHTML repair keeps void elements closed; the Python does not

`html_cleaner.repair_html` parses broken markup with lxml's `HTMLParser` and
serializes with `method='html'`. Most of the time that is fine — a recovered
chapter of ordinary block and inline elements comes back as well-formed XML.

It breaks on void elements. HTML serialization writes them unclosed, so a
chapter containing `<br/>`, `<img/>` or `<hr/>` is recovered as:

```html
<p>Before<br>after an <b>unclosed bold</b></p>
<img src="pic.jpg" alt="a">
<hr>
```

which does not parse as XML — and an EPUB content document is required to be
well-formed XHTML. Line breaks and images are common enough in real books that
this affects a substantial share of malformed chapters, though not all of them.

The Rust port serializes as XML, so void elements stay closed. Verify either
side directly:

```sh
# restore the reference into a scratch directory, then run it
mkdir -p /tmp/ref && git show 7cf9a65:html_cleaner.py > /tmp/ref/html_cleaner.py
python3 -c "import sys; sys.path.insert(0,'/tmp/ref'); from html_cleaner import repair_html; \
    print(repair_html(open('chapter.xhtml','rb').read()).decode())"

cargo run -p epubkit-cli -- repair chapter.xhtml
```

(The reference needs `lxml` and `cssutils` installed to run.)

Two smaller differences in the same step: the Python emits no XML declaration
at all (it serializes the root element rather than the document), and its
strict path uses `pretty_print=True`, which shifts block-level whitespace.

`crates/core/tests/repair.rs` pins the Rust behaviour, including a test that
feeds repaired output back through the parser and asserts it parses strictly.

The choice of *parser* deliberately does match the Python: libxml2's HTML
parser, not its XML parser in recovery mode. Recovering XHTML with the XML
parser silently deletes text — a bare `&` in prose vanishes along with
whatever the parser was mid-way through. `cargo run -p epubkit-core --example
probe -- <file>` prints all four parse/serialize combinations on a given file;
that is the evidence behind the choice.

### Recovered chapters come out as legal XML

HTML allows what XML does not, and libxml2's HTML parser keeps it: `--`
inside a comment, the words after a bare `<` as attribute names (as 2.14
reads one), characters XML forbids written as references. Serialized as XML
anyway, such a chapter was malformed again, so each later pass recovered it
afresh and every run counted it as repaired. The parser also keeps a
stylesheet's text as it stands, the author's `<![CDATA[` included, which the
XML writer then wrapped in a CDATA section of its own; the CSS began with
`<![CDATA[`, and its first rule was lost to it.

The port mends each of these in the recovered tree and then reads the result
back strictly. A chapter that still does not read back is refused, which
leaves it as it was.

Nor can the HTML parser read a DOCTYPE's internal subset: it ends the
DOCTYPE at the subset's first `>`, and the rest of the declarations become
text. The port reads the subset as XML does, declaration by declaration,
reading a parameter entity's declarations where it is used and replacing a
value's character references as it goes. It fills in the entities the subset
declares where the chapter uses them, in text and attribute values, those
they refer to in turn too, but not in a CDATA section, a comment or a
processing instruction, whose text a reference there is. In an attribute
value, what an entity stands for is the value's text, and a quote in it ends
nothing. Each reference is filled in whole or stays as written, within a
bound on what filling in may cost, so a "billion laughs" stays a few
references.

### Malformed chapters are read as UTF-8

The reference's recovery parser left the encoding to libxml2, which obeys a
`<meta>` charset that is often stale, and which in recent releases (2.14) reads
a chapter declaring no charset as ISO-8859-1. Either way valid UTF-8 came out
as mojibake: "ä" as "Ã¤". Upstream fixed this after the fork
(b1rdmania/epubkit#8) by forcing UTF-8 whenever the bytes are valid UTF-8. The
port does the same, so here it matches upstream rather than `7cf9a65`.

It cannot do so by passing the encoding: the `libxml` crate's `encoding`
option is unsound in 0.3.21, freeing the C string it builds before libxml2
reads it. `html::parse_content` instead sets `ignore_enc` and, when there is
anything non-ASCII to decode, prefixes a byte order mark. `tests/encoding.rs`
pins the result, and must pass against libxml2 2.9 and 2.14 alike, which
disagree about the default.

A chapter whose bytes are not UTF-8 is decoded before either parser sees it,
and is parsed as UTF-8 in turn, with its XML declaration and any `<meta>`
charset saying so. A byte order mark decides, then an encoding named in the
XML declaration, then one named in a `<meta>`, and otherwise the bytes are
taken for UTF-8 with stray bytes pasted in, each read as windows-1252. That
is also what browsers take a declared ISO-8859-1 to mean, and it reads a
chapter that is windows-1252 throughout the same way. Upstream still leaves
such a chapter to libxml2, whose HTML parser ignores an encoding named in an
XML declaration: 2.9 reads on as Latin-1 and 2.14 as UTF-8, so a Shift_JIS or
windows-1251 chapter came out as nonsense, and under 2.14 Latin-1 accents came
out as replacement characters. Both releases also read windows-1252's curly
quotes and dashes as Latin-1's invisible control characters.

The same decision now holds for well-formed chapters, which the port first
left to libxml2's XML parser. That parser obeys the declaration: a chapter
declaring ISO-8859-1 kept Word's quotes and dashes as control characters, and
one whose declaration had outlived a re-encoding to UTF-8 came out as "Ã¼".
So a chapter reads the same with or without a markup error in it. And one
stray byte no longer sends a whole UTF-8 chapter through windows-1252, which
had turned every Cyrillic letter into two Latin ones.

ISO-2022-JP is the exception to valid UTF-8 being UTF-8: it is written in
seven bits, so its bytes are valid UTF-8 as well. A chapter that names it and
has the escapes it switches character sets with is read as ISO-2022-JP; read
as UTF-8, its Japanese came out as ASCII gibberish.

Only real declarations count. A legacy chapter's `<meta>` charset is found
by parsing the chapter as it will be parsed once it is decoded: strictly if
it is well-formed, by the HTML parser if not. Its markup is ASCII in any
encoding it can name, so its bytes read as windows-1252 give the parser the
same markup, and only what the parser takes for a `<meta>` counts, whatever
a script, a CDATA section, a comment or a DOCTYPE holds. Only a CDATA section
in the chapter's text is kept from the HTML parser, which does not read it as
the text XHTML takes it for; a `<![CDATA[` in a script, a title or an
attribute value, whose section would end past it, is text to both. Only the `<meta>`'s own attributes count, not another
vocabulary's with the same names, and a `content` names a charset only for an
`http-equiv="Content-Type"`. A pattern over the text found `<meta>`s in
comments and in other `<meta>`s' descriptions, and scanners after it took a
script's `<!--` for a comment, a `</script>` in a CDATA section for the
script's end, and a `>` in a DOCTYPE's comment or entity value for the
DOCTYPE's.

Only real declarations are renamed UTF-8, too: the XML declaration where it
opens the chapter, and a `<meta>` element's `charset`, or the charset in an
`http-equiv="Content-Type"`'s `content`, found on the parsed chapter. The same
words in a CDATA section, a comment or another `<meta>`'s `content` are text,
and stay as written. Renaming by pattern over the text changed them too, and
a `<meta>` written inside the XML declaration's encoding made it panic.

### Prose after `<code>` and `<pre>` is cleaned

lxml stores the text *following* an element as that element's `tail`, so
`text_cleaner` skipping `<code>` also skipped the ordinary prose that came
after it. libxml2 keeps that text in its own sibling node, so only what is
genuinely inside a skipped element is spared. A book with inline `<code>` will
differ here.

### Mojibake is repaired before the other text fixes

`text_cleaner` repairs mojibake after normalizing quotes, so a repaired quote
escaped the normalization every intact one got, and one pattern could never
match at all: "à" read as Latin-1 ends in a no-break space, which quote
normalization had already made a plain one. The port repairs encoding first.
The table is upstream's current one (b1rdmania/epubkit#8), which adds UTF-8
punctuation, "ß", the acute vowels and the capital umlauts to what `7cf9a65`
had. Repairing first made the two patterns ending in a no-break space or a
soft hyphen ("à" and "í") match a real "Ã" too, as in Portuguese "MAÇÃ", so
they are only repaired where no capital comes before.

### Text cleanup leaves correct text alone

Several of the reference's text fixes changed text that was right:

- Removing space before punctuation glued ".45", ".NET" and ".com" to the
  word before, and stripped French spacing before `; : ! ?`. Only plain
  spaces before a mark that ends a word are removed now, and in French text
  only before `.` and `,`. Text is in the language of the nearest element
  that gives one in `xml:lang` or `lang`, as a browser reads it, and in the
  book's where none does: a French quotation in an English book keeps its
  spacing.
- No-break spaces were folded to plain ones, so a scene break written as a
  paragraph holding one collapsed to nothing. They are kept.
- `‚`, which opens a German quote, was folded to a comma, and `„` not at all.
  Both fold to quotes.
- A space was added after any full stop before a capital, splitting "U.S.A."
  and "J.R.R. Tolkien", "1.E.8" and "README.TXT". It is added only between a
  lowercase word and a capitalised one, in a word with no other stop in it.
- Japanese and Chinese doubled ellipses and dashes were folded and then
  shortened, and NFC replaced CJK compatibility ideographs with the unified
  ones they stand for, changing glyphs that names rely on. Both are kept.
- `?!?!` became `!!!`, and `,,Hallo''` lost a comma. `<tt>`, `<var>` and
  MathML were cleaned like prose. Indentation between elements was counted as
  extra spaces.

### CSS is edited in place, not reprinted

`cssutils` is prone to dropping comments and reformatting at-rules, and any
library that parses a stylesheet and prints it back rewords it. An earlier
version of the port did that with `lightningcss`, which printed for current
browsers: `(max-width: 600px)` came back as `(width <= 600px)` and
`transparent` as `#0000`, syntax older reading engines do not read, so the
rules using it stopped applying. Comments and the `@charset` were dropped, and
a minified sheet came back laid out at length.

The port finds rules with `cssparser`'s tokenizer and cuts the ones that go
out of the text where they stand. Everything else is left byte for byte as
the book wrote it. Rule *selection* is unchanged in outline: only top-level
style rules are considered for removal, a rule survives if any part of any of
its selectors is in use, and anything with a pseudo-class, pseudo-element or
attribute selector is kept outright.

Three details differ. Names are read as CSS defines them, so one may begin
with a non-ASCII character; the reference wanted ASCII there, and removed a
rule like `.überschrift` while the book was using it. A selector with an
escaped name, such as `.\31 st` for the class `1st`, is kept outright too,
since reading escapes is beyond this scan, and so is anything else that is
not plain names and combinators. And removing fonts reaches into `@media`
and other grouping rules, where the reference left `@font-face` rules
pointing at the files it deleted.

A `<style>` element's text and CDATA sections are one stylesheet, read in
order, as a reading engine reads them. Removing a font or rewriting a url
edits them as one, then puts each piece back in the section it came from,
so CDATA stays CDATA. Edited one section at a time, a rule split between two
was cut in half, and the common `/*<![CDATA[*/ … /*]]>*/` wrapping hid every
rule inside it. What an entity reference stands for is part of the CSS too,
so `url(&cdn;cover.png)` is read as the url it is, not as `url(cover.png)`.
An entity cannot be edited where it is written, so one an edit runs into is
written out as what it stands for, and edited with the text around it, or
in new text of its own in a `<style>` that is nothing but entities; one
beside an edit stays as written. One declared to stand for nothing is read
as that. One that cannot be read stops the edits that run into it: one a
doctype that is never loaded declares, one whose value is in a file, which
is never loaded either, and one whose value holds either of those, or an
element, whose text is no part of the CSS.

### Empty paragraphs are collapsed among siblings

`normalize_whitespace` tracked runs of empty `<p>`/`<div>` in document order,
so an empty paragraph could pair with an unrelated one elsewhere in the tree
and be dropped. The port groups runs among siblings, which is what
"consecutive empty paragraphs" means.

What counts as empty is narrower too. An element with an id, an `epub:type`
or any attribute but `class` and `style` is a link target or a marker, a
page break the page list points at say, and stays. And a paragraph holding
an entity other than a space, `&bull;` or `&mdash;` under an XHTML 1.1
doctype that is never loaded, has content even though it has no text as such;
the port's libxml2 parse had been dropping scene-break ornaments.

### Direction and visibility are kept

The reference stripped `dir`, `hidden`, `inert` and `popover` along with the
interaction attributes. They change what is shown and how: without `dir` an
Arabic or Hebrew book runs left to right, and without `hidden` a navigation
document in the spine shows its landmarks and page list. The port keeps them.

### Optimized Huffman tables are applied by rewriting, not by the encoder

The reference encodes with `optimize=True`, and so does this port — but not via
`jpeg-encoder`'s own `set_optimized_huffman_tables`, which is a trap.

Turning that flag on also switches the encoder from one interleaved scan to
three single-component scans (`encoder.rs:589` routes to the sequential path
whenever optimization is on, with no way to opt out). That layout is legal
baseline JPEG and libjpeg reads it, but it is rare enough that simpler decoders
mishandle it: `zune-jpeg`, which the `image` crate uses, returns noise for every
such file. The test asserting that a solid white image survives the pipeline
caught it.

The two things are separable, and only the scan split is dangerous. Optimal
tables are what `cjpeg -optimize`, mozjpeg and Pillow all produce, and they keep
the ordinary interleaved scan. So `core::jpeg` takes the finished interleaved
file and rewrites only its Huffman coding — decode the scan to its symbol
stream, count frequencies, build tables per Annex K, re-encode the identical
symbols. No dequantization and no IDCT are involved, so nothing is
approximated; SOF, DQT, scan header, component order and every DCT coefficient
are copied through byte for byte. It is what `jpegtran -optimize` does.

One more thing changes, as libjpeg would have it. An interleaved scan codes
whole MCUs, so where an image's luma does not fill out the last one, 4:2:0
greyscale being the case here, blocks past the image's edge are coded that no
decoder shows. libjpeg codes each as the DC of the block before it and no AC;
`jpeg-encoder` fills them with the image's last row or column repeated, which
in a dithered image cost 0.4% of a page-sized file and a quarter of a strip
eight pixels high. The rewrite codes them as libjpeg does.

The output is therefore built as the reference's is, but for two legal
differences that come from `jpeg-encoder`: its frame header comes before its
quantization tables, and its components are numbered 0 to 2 rather than 1 to
3. No decoder tried minds either, but they are the first thing to look at if
a reader ever cannot show an image. The rewrite is verified three ways: the
rewritten file must decode to the same pixels, must be smaller, and must
re-decode to the exact symbol stream it was built from — that last check runs
inside `optimize_huffman` itself, so a bug degrades to "no saving" rather than
to a corrupt book. Anything unrecognized (progressive, restart markers,
multiple scans, 12-bit) is declined and the original kept. Set
`EPUBKIT_JPEG_TRACE=1` to see why a file was declined.

Measured against the unoptimized file, on images that have been through the
full pipeline: ~7.5% on photographic content, ~6% on line art, ~3% on a page of
text, ~4% on 4-level dithered noise, and 40–55% on near-empty images. Sizes
land within 0.3% of what libjpeg produces from the same pixels at the same
quality, whose quantization tables are the same.

### Pixel operations are checked against Pillow, not eyeballed

`tests/fixtures/` holds input/output pairs generated by running Pillow
directly, so the reproductions of `convert("L")`, `ImageOps.autocontrast` and
`ImageEnhance.Contrast` cannot drift unnoticed. Regenerate them with the script
recorded in the commit that added them.

Three of Pillow's behaviours are reproduced exactly because they are decisions
rather than accidents:

- **Grayscale** is ITU-R BT.601 in Pillow's fixed-point form,
  `(R*19595 + G*38470 + B*7471 + 32768) >> 16`, verified over 160,608 samples.
  Rust imaging crates default to Rec. 709, which differs by 10 grey levels on
  average and 33 at worst — enough to move a pixel across a quantization
  threshold when there are only four levels.
- **Contrast** blends against a solid fill of the image's *own mean luma*, not
  against mid-grey. The obvious `(v - 128) * f + 128` is a different operation
  on any image that is not mid-grey on average.
- **Autocontrast** clips a percentage off each histogram end by a specific
  integer walk, then rescales between the surviving endpoints.

Two things are deliberately not bit-exact, because chasing them buys nothing
visible: Lanczos resampling (same algorithm — the `image` crate scales the
filter kernel on downscale exactly as Pillow does — but `f32` coefficients
rather than fixed-point) and error diffusion (classic Floyd–Steinberg on the
grey channel, where Pillow diffuses against a palette in RGB).

### Photos are turned the way their EXIF data says

The reference decoded an image as stored and dropped its EXIF data,
orientation tag and all. A phone photo stored sideways, with a tag saying
to turn it a quarter clockwise, is shown upright by a reader, but came out
of the reference sideways for good; in Light Novel mode, taken for
landscape art and turned the other way, it came out upside down. The port
turns each image as its orientation tag says (JPEG, PNG, WebP and TIFF can
carry one) before anything looks at its shape.

### Images are decoded by the `image` crate, not Pillow

The two read what a book's images are normally in alike: JPEG (progressive,
CMYK and YCCK included), PNG, GIF, WebP and BMP. A few rarer kinds Pillow
reads, the `image` crate does not: arithmetic-coded JPEG, and TIFF that is
fax-compressed (CCITT G3, or G4 written least significant bit first),
JPEG-compressed, paletted, or grey at 2 or 4 bits. Two more TIFF variants it
reads differently: an extra channel marked "unspecified" is taken for alpha,
and premultiplied alpha is not undone. An image that cannot be decoded stays
in the book as it was, and the summary counts it. TIFF is not among the image
types EPUB requires a reader to show, and few readers decode arithmetic-coded
JPEG either.

Both refuse an image of more than 178,956,970 pixels as a likely
decompression bomb. The `image` crate on its own refuses one whose decoded
pixels take more than 512 MiB, which left a 12000 x 12000 RGBA image, 144
million pixels, unconverted at full size; the port lets decoding allocate
enough for an image at Pillow's limit, at sixteen bits a channel.

### Cover generation is omitted

`generate_cover_image` drew a title/author cover for books that lack one. It is
not ported: it needs text rendering, which needs a bundled typeface, and the
reference's own fallback (`ImageFont.load_default()`) produces an unreadable
bitmap-font cover on any machine without DejaVu or Helvetica. A book with no
cover comes out with no cover.

### Converted images never overwrite each other

The reference names every converted image `stem.jpg` and writes it without
looking, so `cover.png` and `cover.jpeg` both became `cover.jpg` and one
silently replaced the other (upstream's issue #11), as did `plate.png` and an
existing `plate.jpg`. A case-insensitive filesystem made it worse: `IMG.JPG`
was written as `IMG.jpg`, which there is the same file, and then deleted as
the old one.

The port gives each output a name nothing else in its directory has,
ignoring case, appending `-2`, `-3`… where it must, and an image that converts
to its own name keeps its exact spelling. Since images sharing a filename can
now be renamed differently, a reference is followed by its path from the
document that makes it, falling back to the bare filename only when the path
leads nowhere and the filename is unambiguous. Only the filename in a
reference changes; its directory, fragment, percent-encoding and quotes stay
as written, and links to other sites are left alone. A `srcset` is split
into candidates as the HTML standard splits it, so a URL with a comma in it
is one URL, and a `data:` URL or another site's is left whole.

### Converted images are counted and declared as what they became

The reference's summary counted images by the first thing said about each,
meant to be the format change (its own comment gives
`{"PNG→JPEG": 5, "baseline JPEG": 3}`), but for a JPEG that was how it was
resized, so a book of JPEGs listed one entry per size. The port counts by
the format change, a JPEG written again as `baseline JPEG`. And an image
converted under its own name, a PNG named `plate.jpg` say, is declared
`image/jpeg` in the manifest as a renamed one is; the reference changed the
media type only of an image it renamed.

### Images are known by what they are, not by their names

The reference tried only images named `.png`, `.gif`, `.webp`, `.bmp`,
`.jpg`, `.jpeg`, `.tif` or `.tiff`, and passed over the rest in silence: a PNG
the manifest declared with no extension, or as `spread.bin`, or a JPEG named
`.jpe` or `.jfif`, stayed as it was, and the summary did not mention it. The
port tries every image the manifest declares but an SVG, known by its media
type or its name, and reads each one's format from its bytes. The converted
file takes the name with `.jpg` for whatever extension it had, references
follow it, and the summary counts it by what it was: a PNG named `plate.jpg`
is a `PNG→JPEG`. One no decoder reads is left as it was and counted as such.
The summary's total is these images, the ones in the book that are not SVG,
so it no longer counts SVG images, which are drawn and never converted, or
images the manifest declares and the book lacks.

### Images are converted on every core

The reference converted one image after another. The port converts as many
at once as the machine runs threads, each holding while it converts about as
much memory as its image needs, out of 1 GiB for all of them, so very large
images wait their turn. Each image is then named, written and reported in the
manifest's order, so the book that comes out is the same whatever order they
finished in.

### Light Novel mode keeps every page it makes

The reference split a double-page spread into two images but pointed the book
at only the last, the left half; the right half, which comes first, was
packaged and never shown. The port declares every page in the manifest and
shows them in reading order where the spread was, each the way the spread
was shown, minus the `width` and `height` that described it, and minus any
`srcset`, `sizes` or `<picture>` sources, which would show one image on every
page. An SVG wrapper,
common around full-page illustrations and sized to the spread in its viewBox,
gives way to a plain image per page. A rotated image sheds its old size and
wrapper the same way. The report counts a split spread as one image.

### Light Novel mode reshapes only pages of art

The reference turned or split every image wider than tall, however the book
showed it. Most ways of showing an image cannot take one of another shape:
an SVG document or a CSS background showed the first half of a split image
and nothing showed the second, an SVG illustration's labels no longer lay
over what they labelled, a small image in a line of text was split in two in
the line or stood on end, a heading's image read right half first, and the
cover lay on its side in a reader's library.

Before converting images in Light Novel mode, the port reads how the book
shows each one, and reshapes only an image shown as a page of its own: an
`<img>` outside a heading with nothing else in its line, or an SVG that shows
nothing but its image, on its own the same way. Anything else that names
the file keeps its shape: an SVG that draws more, an SVG document, a
stylesheet or `style`, an image in text or a heading, a link to the file, a
`srcset` other than the image's own, and the cover. For this the chapters are
repaired before the image step rather than after it, which changes nothing
else, since neither step reads what the other writes.

Nor is an image reshaped in a box the book's CSS sizes for it, as a split
image's pages take more room down the page than it did and a turned one is
another shape. A box around it of a set height or `max-height`, in pixels,
ems or the like or a screen high in `vh`, or of a set `aspect-ratio`, cut the
second page off or let it run over what came after. A `transform`, or
`position: absolute` or `fixed`, on the box or the image turned the pages
again or laid them over each other. And the image's own height, which each
page keeps, gave every page its size. The CSS is read from `style`
attributes, `<style>` elements and every stylesheet in the book, and a rule
counts if the last part of its selector could name the element, whatever
else it asks, as long as it is not for a pseudo-element, a hover or focus, or
print only. A height that is a percentage is of the page, as when a plate is
fitted to it, or of a box this finds, and does not count. Reading the cascade
no further keeps an image whole wherever a rule might frame it.

Even shown as a page, an image is reshaped only if that shows it at least
15% bigger: the panel never enlarges an image, so one it already shows whole,
a small figure or an ornament on a line of its own, gains nothing from being
turned, and nor does one nearly square. And an image more than 2.6 times as
wide as it is tall, wider than two pages side by side, is a rule or a banner
and stays whole; the reference split a 600 x 10 rule into two.

### One unreadable file does not sink the book

A chapter nothing can parse, an empty or blank file say, is left exactly as
it was, as the reference left it, and the report counts it; every other step
works on the chapters that did parse. libxml2 2.14 "recovers" an empty file
into a document with no root element, which would serialize to a bare
declaration, so that counts as unreadable too.

Stylesheets are decoded as browsers decode them, from a byte order mark, then
an `@charset` naming a legacy encoding; otherwise as UTF-8 if the bytes are
valid UTF-8, and as windows-1252 if not. The reference read them as UTF-8 and
dropped what did not fit. One the run rewrites is saved as UTF-8, its
`@charset` rewritten to match; one it leaves alone keeps its bytes.

### Output names follow upstream's later filename options

Upstream added a choice of output name after the reference commit, in its
PR #5: the original filename, Title - Author, Author - Title, Title, or a
template filled from `{title}`, `{author}`, `{year}`, `{series}`,
`{series_index}`, `{language}` and `{original}`. The port follows its rules:
doubled braces are literal, a field it does not know or a formatting option is
refused, and a template ending in `.epub` does not get a second one. Author -
Title, the default, is the name the reference always gave.

The choice is remembered with the settings rather than made per upload, and
both front ends check a template before any book is touched. Since a book can
now be named exactly as the file it came from, neither front end ever
replaces a file that is already there; the output becomes `name (2).epub`.
The CLI used to write over one without asking.

### Paths in a book stay inside it

The reference joined manifest hrefs, and the package path in
`container.xml`, to the unpacked book as they stood. An absolute href, or one
climbing out with `..`, let a book have the pipeline rewrite or delete files
that were not part of it: a "font" deleted, a "chapter" repaired over. The
port resolves every such path within the book, a leading `/` from the book's
root as URLs inside an EPUB container resolve, and ignores one that leads out.
A `container.xml` naming a package that is not in the book falls back to the
search for one that is.

### Encryption metadata is parsed, not searched

The reference decided whether a book had DRM by looking for namespace URIs in
`META-INF/encryption.xml` read as UTF-8 text. The same file written in UTF-16,
as XML allows, had none of them to find, and a book with encrypted chapters
went through the pipeline as if it had none. The port parses the file and
judges each entry by its algorithm and the file it names: only a font under the
IDPF's or Adobe's obfuscation is let through. A file that cannot be parsed is
taken for DRM.

### Only an SVG that just shows its image is unwrapped

The reference took any SVG in the first three chapters holding exactly one
`<image>` for a cover wrapper and replaced it with a plain `<img>`. One that
also held text or shapes, a labelled map say, lost them. The port unwraps an
SVG only when the image is all it draws, beside a title or a description, and
leaves an illustration alone. Light Novel mode does the same: an illustration
stays, and its image keeps its shape.

Nor does it unwrap one that shows only part of its image: a viewBox over the
right half of a spread, an image slid out of its box, or one sliced to fill a
box of another shape. An `<img>` in its place showed the whole picture. The
port replaces only an SVG whose box is its image's own: a viewBox from the
origin the image's size, or none and an image filling the SVG, with nothing
transforming, clipping or fading it.

### The HTML repair pass runs earlier

The reference repaired chapters *after* rewriting image references. But
reference rewriting parses with the same recovering parser and writes the file
back, so it silently repaired each chapter first — and the repair step then
found nothing to do. Repair now runs before anything else touches a chapter,
which makes the count meaningful and means every later step sees a well-formed
tree.

### The OPF is parsed once

The reference re-read and re-wrote the package document at nearly every step.
The port parses it once, applies every edit to that one document, and writes it
once before repackaging.

### Optimized books can be larger than the originals

This is not a defect, and it is not specific to the port — the reference does
the same. Dithering to four levels is high-frequency noise by construction,
which is the worst case for a DCT codec. Measured with Pillow on a smooth
gradient cover downscaled to 480x800: 82 KB as a dithered JPEG against 6.6 KB
as a smooth grayscale one.

Books of photographic artwork fare better, since the source is already noisy
and was already a large JPEG. But a book of clean line art or flat colour can
come out several times bigger, so the report says "increase" rather than
printing a negative reduction.

Worth revisiting at some point: storing a carefully dithered image in a *lossy*
format is self-defeating, since the codec blurs the very pattern the dither
paid to produce. The device spec calls for JPEG, so that is what is emitted.

### A broken table of contents is actually repaired

`fix_toc` in the reference detects NCX entries pointing at files that do not
exist, calls `_fix_ncx_references` to repair them, writes the file and reports
`Fixed N broken TOC references`. But `_fix_ncx_references` is `pass` — an empty
stub with a comment saying regeneration will handle it, which at that point in
the flow it never reaches. So the book keeps its broken TOC and the report says
it was fixed.

The port regenerates the NCX from the spine in that case, which is what the
stub's comment intended, and reports `TocOutcome::Generated`.

When the reference did generate an NCX, it copied each chapter's href into it
as the OPF wrote it: relative to the OPF. An NCX in a folder of its own,
`Navigation/toc.ncx` say, then pointed every entry at a file that was not
there. The port writes each link relative to the NCX. It also declares a new
NCX under an id nothing else in the package has; the reference always used
`ncx`, which a chapter may already be called.

### Validation collects all problems

`is_valid_epub` returned on the first problem. `package::validate_epub` returns
every problem it finds, which is more useful when diagnosing a book.

### Packaging is deterministic

The Python walked the tree in `os.walk` order, sorting only within each
directory. The Rust sorts the full path list, so the same input directory
always produces a byte-identical archive.

## Still open

- Whether saved presets should capture the device (`x4`/`x3`) or keep it as a
  separate sticky setting. Current thinking: keep it separate — it is a
  property of the hardware, not of a processing profile.
- Whether to vendor libxml2 or depend on a system copy, decided when Windows
  packaging starts.
- libxml2's upstream security-maintenance status is worth re-checking before
  release, since this code parses untrusted files. The parser is already
  configured to refuse network access and to leave entity references
  unexpanded; see `core::xml::hardened_options`.
