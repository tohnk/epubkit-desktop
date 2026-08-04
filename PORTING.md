# Porting epubkit to Rust

This repository is being converted from the original Python/FastAPI web app
into a native desktop application: a Rust core, a thin CLI for validation, and
eventually a Tauri front-end reusing the existing HTML/CSS.

The Python implementation is still present and still the reference. Nothing
should be deleted from it until the Rust port produces equivalent output on a
corpus of real books.

## Layout

```
crates/core/    epubkit-core — the pipeline, as a library
crates/cli/     epubkit-cli  — `epubkit`, a thin driver for testing the core
```

Module names mirror the Python ones so the two can be read side by side:

| Python             | Rust                    | Status |
|--------------------|-------------------------|--------|
| `epub_packager.py` | `core::package`         | ported |
| `html_cleaner.py`  | `core::html` (repair)   | repair only |
| —                  | `core::xml`             | new: shared libxml2 wrapper |
| `epub_structure.py`| —                       | not started |
| `metadata_handler.py` | —                    | not started |
| `image_processor.py`  | —                    | not started |
| `text_cleaner.py`  | —                       | not started |
| `epub_processor.py`| —                       | not started |

## Build prerequisites

The XHTML repair step links against libxml2 — the same C library lxml wraps,
chosen so the port's parse/serialize behaviour stays comparable to the
reference implementation.

```sh
# Debian/Ubuntu
apt-get install libxml2-dev
```

macOS ships a libxml2 in the SDK. Windows needs one built (vcpkg or cmake);
when the Tauri packaging work starts, vendoring and statically linking it is
likely the better answer than depending on a system copy.

## Using the CLI

```sh
cargo run -p epubkit-cli -- info      book.epub
cargo run -p epubkit-cli -- validate  book.epub
cargo run -p epubkit-cli -- roundtrip book.epub -o out.epub
cargo run -p epubkit-cli -- repair    chapter.xhtml
```

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

### XHTML repair emits XHTML; the Python emits HTML

`html_cleaner.repair_html` parses broken markup with lxml's `HTMLParser` and
serializes with `method='html'`. On a malformed chapter that produces:

- an HTML 4.0 Transitional doctype the book never had,
- an unclosed `<meta http-equiv="Content-Type">` injected into `<head>`,
- the original XML declaration stranded *after* the doctype.

The result is not well-formed XHTML, which is what an EPUB content document is
required to be. The Rust port serializes as XML, strips the synthesized doctype
and the demoted XML declaration, and emits one correct declaration — so
recovered files come out well-formed. `crates/core/tests/repair.rs` pins this,
including a test that feeds repaired output back through the parser and asserts
it parses strictly.

The choice of *parser* deliberately does match the Python: libxml2's HTML
parser, not its XML parser in recovery mode. Recovering XHTML with the XML
parser silently deletes text — a bare `&` in prose vanishes along with
whatever the parser was mid-way through. `cargo run -p epubkit-core --example
probe -- <file>` prints all four parse/serialize combinations on a given file;
that is the evidence behind the choice.

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
