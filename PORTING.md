# Porting epubkit to Rust

This repository is being converted from the original Python/FastAPI web app
into a native desktop application: a Rust core, a thin CLI for validation, and
a Tauri front-end reusing the original HTML and CSS.

The Python implementation has been removed now that the port is complete. It
survives in git history at `7cf9a65`, which is the pinned reference for any
comparison — see "Validating against the reference" below.

`static/` and `templates/` are deliberately kept: that is the UI the desktop
front-end will reuse, and none of it is Python.

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
  doing nothing.

## Using the CLI

```sh
cargo run -p epubkit-cli -- info      book.epub
cargo run -p epubkit-cli -- validate  book.epub
cargo run -p epubkit-cli -- roundtrip book.epub -o out.epub
cargo run -p epubkit-cli -- repair    chapter.xhtml
cargo run -p epubkit-cli -- optimize  book.epub
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
every preset change.

On the CLI, saved settings are the base and the flags are overrides: each
`--no-*` flag can only turn something off, so an option nobody mentioned keeps
whatever it had. Metadata edits (`--title`, `--author`) are about one book and
are never persisted.

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

### Prose after `<code>` and `<pre>` is cleaned

lxml stores the text *following* an element as that element's `tail`, so
`text_cleaner` skipping `<code>` also skipped the ordinary prose that came
after it. libxml2 keeps that text in its own sibling node, so only what is
genuinely inside a skipped element is spared. A book with inline `<code>` will
differ here.

### CSS goes through a real parser

`cssutils` is prone to dropping comments and reformatting at-rules. The port
uses `lightningcss`, so comments, `@import` and `@media` blocks survive a
round-trip. Rule *selection* is unchanged: only top-level style rules are
considered for removal, a rule survives if any part of any of its selectors is
in use, and anything with a pseudo-class, pseudo-element or attribute selector
is kept outright.

Note that `lightningcss` is pre-1.0 (currently an alpha), so its API may move
under a future upgrade. It is confined to `core::css`.

### Empty paragraphs are collapsed among siblings

`normalize_whitespace` tracked runs of empty `<p>`/`<div>` in document order,
so an empty paragraph could pair with an unrelated one elsewhere in the tree
and be dropped. The port groups runs among siblings, which is what
"consecutive empty paragraphs" means.

### Image output omits optimized Huffman tables

The reference encodes with `optimize=True`. Those files are perfectly valid —
libjpeg reads them correctly — but the `image` crate's own decoder returns
noise for them, which was caught by a test asserting that a solid white image
survives the pipeline.

A decoder that trips over a standard-but-uncommon construct is exactly what a
reader running on an ESP32 is likely to be, and the saving is a few percent on
real images. The option is off, with a regression test pinning it. Re-enable it
only with hardware to test on.

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

### Cover generation is omitted

`generate_cover_image` drew a title/author cover for books that lack one. It is
not ported: it needs text rendering, which needs a bundled typeface, and the
reference's own fallback (`ImageFont.load_default()`) produces an unreadable
bitmap-font cover on any machine without DejaVu or Helvetica. A book with no
cover comes out with no cover.

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
