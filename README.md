# epubkit

An EPUB optimizer for e-ink readers, as a desktop app and a command-line tool. Drop in any EPUB and get back a clean, optimized file ready for your device.

Originally built for the [Xteink X4](https://xteink.com/) (480x800 portrait e-ink display, 4-level grayscale, SSD1677 controller, ESP32-C3). Now also supports the smaller [Xteink X3](https://www.xteink.com/products/xteink-x3) (528x792 display, same controller) via a device toggle, and works with any Xteink reader or e-ink device that supports EPUB.

## Processing pipeline

epubkit runs this pipeline on every EPUB:

| Step | What it does |
|------|-------------|
| 1 | **DRM check** — detects DRM-protected files and stops early with a clear message |
| 2 | **Extract** — unpacks the EPUB ZIP structure into a working directory |
| 3 | **Parse structure** — locates the OPF package file and parses the manifest |
| 4 | **Apply metadata edits** — overwrites title/author if you edited them in the window or passed `--title`/`--author` |
| 5 | **Find content files** — catalogs all XHTML, CSS, image, and font files in the EPUB |
| 6 | **Process images** — converts all images to baseline JPEG, resizes to the device screen (480x800 for X4, 528x792 for X3; max 1024x1024), applies 4-level grayscale quantization with Floyd-Steinberg dithering, autocontrast histogram stretching, and contrast boost. Light Novel mode rotates/splits landscape images. Converted images never overwrite one another |
| 7 | **Repair HTML + strip attributes** — fixes malformed XHTML with libxml2's recovery parser (reading it as UTF-8, which EPUB requires), strips unnecessary attributes (data-\*, aria-\*, role, tabindex, etc.) to reduce parsing overhead for the 380KB RAM device. A chapter that cannot be read at all is left as it was |
| 8 | **Fix SVG covers** — unwraps SVG-wrapped cover images (common in Gutenberg/store EPUBs) |
| 9 | **Update references** — rewrites all internal hrefs and srcs to match renamed image files |
| 10 | **Show reshaped pages** — in Light Novel mode, puts both halves of a split spread in the book, in reading order, and drops the sizes that described the original image |
| 11 | **Remove unused CSS** — collects all used classes/IDs/elements across XHTML files, then strips CSS rules that don't match anything |
| 12 | **Remove embedded fonts** — deletes @font-face rules from CSS, removes font files (.ttf, .otf, .woff, .woff2), and cleans them from the OPF manifest |
| 13 | **Normalize whitespace** — strips excessive empty paragraphs/divs, adds CSS page-break-before to chapter headings (h1, h2) |
| 14 | **Text cleanup** — scans all text nodes (skipping script/style/pre/code) and fixes: double spaces, OCR ligature artifacts (fi/fl/ffi/ffl/ff), smart quotes → straight quotes, mojibake encoding errors, punctuation issues, Unicode NFC normalization |
| 15 | **Clean metadata** — strips store-specific tags (Calibre, iBooks, Kindle, Amazon, Google Play, Kobo) |
| 16 | **Fix TOC** — validates the Table of Contents, generates one from chapter headings if missing |
| 17 | **Clean OS artifacts** — removes .DS_Store, Thumbs.db, __MACOSX, desktop.ini, etc. |
| 18 | **Repackage** — rebuilds the EPUB ZIP with correct mimetype entry and deflate compression |
| 19 | **Output filename** — names the file from its metadata: `Author - Title.epub` by default, or Title - Author, Title, the original filename, or a template of your own. A file already there is never replaced |

## Usage

```sh
# the desktop app
cargo run -p epubkit-desktop

# or the command line
cargo build --release

# optimize a book; the output is named from its metadata
./target/release/epubkit optimize book.epub

# pick a device and a preset
./target/release/epubkit optimize book.epub --device x3 --preset quick

# name the output another way: original, title-author, author-title, title
./target/release/epubkit optimize book.epub --filename original

# or from a template
./target/release/epubkit optimize book.epub --filename-template '{series} {series_index} - {title}'

# inspect a book without changing it
./target/release/epubkit info book.epub
```

Your choices are remembered between runs, so an option you turn off stays off,
and books keep being named the way you last chose.
`epubkit settings show` prints the current state, and `epubkit settings save
"My X4"` keeps it as a named preset.

Building needs libxml2 and pkg-config:

```sh
apt-get install libxml2-dev          # Debian/Ubuntu
brew install pkgconf libxml2         # macOS
```

The desktop app additionally needs a webview on Linux
(`libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev`); macOS and Windows use the
system one.

### On Linux, as a Flatpak

The Flatpak carries everything the desktop app needs, so nothing has to be
installed beyond Flatpak itself, with the [Flathub](https://flathub.org/setup)
remote added (most distributions add it for you). Every push builds one:
download `epubkit.flatpak` from the artifacts of the latest run of the
[Flatpak workflow](https://github.com/tohnk/epubkit-desktop/actions/workflows/flatpak.yml),
then

```sh
flatpak install --user epubkit.flatpak   # fetches the GNOME runtime from Flathub
flatpak run io.github.tohnk.epubkit
```

To build it yourself, with `flatpak-builder` installed and Flathub added:

```sh
flatpak-builder --user --install-deps-from=flathub --install --force-clean \
    build-dir flatpak/io.github.tohnk.epubkit.yml
```

The sandboxed app has no access to your files beyond what you give it: books
come in through the file picker or by dropping them on the window, and the
output folder is the one you choose. It keeps its settings apart from the
command line's, under `~/.var/app/io.github.tohnk.epubkit`.

## Processing presets

| Preset | Images | Text | Fonts | CSS | Metadata | Best for |
|--------|--------|------|-------|-----|----------|----------|
| Quick  | Yes    | Yes  | No    | No  | No       | Fast image + text pass |
| Full   | Yes    | Yes  | Yes   | Yes | Yes      | Complete device optimization |
| Custom | Pick   | Pick | Pick  | Pick| Pick     | Fine-grained control |

The device toggle (X4/X3) works independently of the preset — it controls image dimensions and grayscale depth. So does the choice of output filename.

## Device specs

The optimizer is tuned for these hardware constraints:

| Spec | X4 | X3 |
|------|----|----|
| Display | 480x800 portrait (4.3" panel) | 528x792 portrait (3.7" panel) |
| Grayscale | 4 levels (SSD1677): black, dark gray, light gray, white | Same 4-level SSD1677 hardware |
| Processor | ESP32-C3, 160MHz | ESP32-C3 |
| RAM | 380KB usable | 380KB usable |
| Max image | 1024x1024 pixels | 1024x1024 pixels |
| Formats | EPUB, XTC, XTCH, Markdown, TXT | EPUB, TXT |
| Storage | 32GB + microSD | 16GB microSD |

Image fit boxes match the [CrossPoint reference converter](https://github.com/crosspoint-reader/crosspoint-reader) device profiles (X4: 480x800, X3: 528x792). The panels scan in landscape, but the readers display portrait — images sized to the portrait box render sharp without reader-side upscaling.

Note: stock Xteink firmware (X3 and X4 alike) does not render images inside EPUBs at all — hardware-verified on both devices with unmodified store EPUBs and a diagnostic EPUB covering 8 encoding variants (JPEG parameter permutations, PNG, BMP); every image page renders blank while text renders normally. Image optimization therefore benefits custom firmware such as [CrossPoint](https://github.com/crosspoint-reader/crosspoint-reader)/CrossInk, which renders 4-level grayscale images on both panels.

## Image processing details

- **Format**: All images converted to baseline JPEG (progressive breaks many e-ink readers)
- **Resize**: Fit within the device screen (480x800 X4, 528x792 X3, portrait), hard clamp at 1024x1024
- **Grayscale**: 4-level quantization matching the SSD1677 palette (0, 85, 170, 255) with Floyd-Steinberg dithering (both devices)
- **Contrast**: Auto-histogram stretching (Pillow's `ImageOps.autocontrast`, reproduced exactly) followed by 1.5x contrast boost
- **Subsampling**: 4:2:0 for grayscale (all RGB channels identical, saves ~15-20%), 4:4:4 for color
- **Transparency**: Alpha composited onto white background
- **Light Novel mode**: Landscape images rotated 90°; double-page spreads (aspect > 1.8) split into two portrait pages, both shown in reading order

## Text cleanup details

Scans all XHTML text nodes (skipping `<script>`, `<style>`, `<pre>`, `<code>`):

- **Whitespace**: Multiple spaces/tabs → single space, removes spaces before punctuation
- **OCR ligatures**: fi (U+FB01), fl (U+FB02), ffi (U+FB03), ffl (U+FB04), ff (U+FB00) → plain ASCII
- **Smart quotes**: Typographic quotes/dashes → straight equivalents
- **Mojibake**: Detects and repairs common UTF-8/Latin-1 double-encoding patterns
- **Punctuation**: 4+ dots → ellipsis, missing space after sentence-ending punctuation, duplicate commas
- **Unicode**: NFC normalization

## Tech stack

Rust: a library, a command-line tool, and a desktop app.

- **[Tauri](https://tauri.app/)** — the desktop window, drawn by the system's own webview
- **[libxml2](https://gitlab.gnome.org/GNOME/libxml2)** — XML/XHTML parsing and repair, via the `libxml` crate
- **[image](https://crates.io/crates/image)** — decoding and Lanczos resampling
- **[lightningcss](https://lightningcss.dev/)** — CSS parsing and cleanup
- **[zip](https://crates.io/crates/zip)** — EPUB container handling

This began as a Python/FastAPI web app. The port is described in
[PORTING.md](PORTING.md), which also records where the Rust deliberately
behaves differently from the original. The Python implementation lives on in
git history at `7cf9a65` if you need to compare against it:

```sh
git show 7cf9a65:epub_processor.py
```

## DRM note

epubkit cannot process DRM-protected EPUBs. It will detect DRM and let you know. You'll need to remove DRM first using tools like [DeDRM](https://github.com/noDRM/DeDRM_tools) with Calibre.

## Acknowledgements

Inspired by and built on ideas from:

- [zgredex/baseline_jpg_converter](https://github.com/zgredex/baseline_jpg_converter) — Calibre plugin for baseline JPEG conversion
- [CrossPoint Reader PR #1224](https://github.com/nicnocquee/CrossPoint-Reader/pull/1224) — in-browser EPUB converter with Light Novel mode
- [kxrz/calibre_workflow](https://github.com/kxrz/calibre_workflow) — Calibre plugin for HTML repair and CSS cleanup
- [bigbag/papyrix-reader](https://github.com/bigbag/papyrix-reader) — Xteink device specifications and documentation

## About

Built by [@b1rdmania](https://github.com/b1rdmania). Made because existing tools required too many steps — Calibre plugins, CLI scripts, manual image conversion. epubkit does it all in one pass.

This fork, [tohnk/epubkit-desktop](https://github.com/tohnk/epubkit-desktop), ports it from a Python web app to Rust, as a desktop app and a command-line tool.

## License

MIT — see [LICENSE](LICENSE).
