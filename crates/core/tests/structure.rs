mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use epubkit_core::structure::{
    add_image_to_opf, build_rename_map, declare_reshaped_pages, find_content_files, fix_svg_covers,
    fix_toc, manifest_items, resolve_href, show_reshaped_pages, spine_hrefs, update_css_references,
    update_opf, update_opf_remove_fonts, update_xhtml_references, Renames, ReshapedPages,
    TocOutcome,
};
use epubkit_core::xml;

fn opf(body: &str) -> libxml::tree::Document {
    xml::parse_strict(body.as_bytes()).expect("fixture should parse")
}

/// Where a book with no files on disk is said to be unpacked, for steps that
/// only resolve its paths.
fn book() -> &'static Path {
    Path::new("/book")
}

fn rename_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

const MIXED_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="ch1" href="text/chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="text/chapter2.xhtml" media-type="application/xhtml+xml"/>
    <item id="css" href="styles/main.css" media-type="text/css"/>
    <item id="img" href="images/plate.png" media-type="image/png"/>
    <item id="fnt" href="fonts/body.otf" media-type="font/otf"/>
    <item id="fnt2" href="fonts/legacy.ttf" media-type="application/octet-stream"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="misc" href="extra.bin" media-type="application/octet-stream"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
    <itemref idref="missing"/>
  </spine>
</package>
"#;

#[test]
fn reads_the_manifest() {
    let items = manifest_items(&opf(MIXED_MANIFEST)).unwrap();
    assert_eq!(items.len(), 8);
    assert_eq!(items[0].id, "ch1");
    assert_eq!(items[0].href, "text/chapter1.xhtml");
    assert_eq!(items[0].media_type, "application/xhtml+xml");
}

#[test]
fn spine_skips_dangling_idrefs() {
    let spine = spine_hrefs(&opf(MIXED_MANIFEST)).unwrap();
    assert_eq!(
        spine,
        vec![
            ("ch1".to_string(), "text/chapter1.xhtml".to_string()),
            ("ch2".to_string(), "text/chapter2.xhtml".to_string()),
        ],
        "the idref with no manifest entry should be dropped"
    );
}

#[test]
fn classifies_content_files_by_media_type() {
    let files =
        find_content_files(Path::new("/book"), Path::new("/book"), &opf(MIXED_MANIFEST)).unwrap();

    assert_eq!(files.xhtml.len(), 2);
    assert_eq!(files.css, vec![Path::new("/book/styles/main.css")]);
    assert_eq!(files.images, vec![Path::new("/book/images/plate.png")]);
    assert_eq!(files.ncx, vec![Path::new("/book/toc.ncx")]);
    assert_eq!(files.other, vec![Path::new("/book/extra.bin")]);
}

/// A font mislabelled as octet-stream still has to be found, or the font
/// removal step silently leaves it in the book.
#[test]
fn classifies_fonts_by_extension_when_the_media_type_lies() {
    let files =
        find_content_files(Path::new("/book"), Path::new("/book"), &opf(MIXED_MANIFEST)).unwrap();
    assert_eq!(
        files.fonts,
        vec![
            Path::new("/book/fonts/body.otf"),
            Path::new("/book/fonts/legacy.ttf"),
        ]
    );
}

/// An SVG document is known by its media type, whatever its name; and by its
/// name where the book gives it some other media type, since a wrong guess
/// only costs a parse that fails.
#[test]
fn svg_documents_are_known_by_media_type_or_name() {
    let manifest = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="diagram" href="images/diagram" media-type="image/svg+xml"/>
    <item id="map" href="images/Map.SVG" media-type="application/octet-stream"/>
    <item id="plate" href="images/plate.png" media-type="image/png"/>
  </manifest>
</package>
"#;
    let files = find_content_files(book(), book(), &opf(manifest)).unwrap();

    assert_eq!(
        files.svg,
        vec![
            Path::new("/book/images/diagram"),
            Path::new("/book/images/Map.SVG")
        ]
    );
}

#[test]
fn rename_map_keeps_the_directory() {
    let processed = rename_map(&[
        ("images/plate.png", "plate.jpg"),
        ("cover.png", "cover.jpg"),
        ("already.jpg", "already.jpg"),
    ]);

    let map = build_rename_map(&processed);

    assert_eq!(map.get("images/plate.png").unwrap(), "images/plate.jpg");
    assert_eq!(map.get("cover.png").unwrap(), "cover.jpg");
    assert!(
        !map.contains_key("already.jpg"),
        "an unchanged name is not a rename"
    );
}

#[test]
fn manifest_hrefs_follow_renamed_images() {
    let doc = opf(MIXED_MANIFEST);
    let map = rename_map(&[("images/plate.png", "images/plate.jpg")]);

    assert_eq!(
        update_opf(&doc, &Renames::new(book(), book(), &map)).unwrap(),
        1
    );

    let item = manifest_items(&doc)
        .unwrap()
        .into_iter()
        .find(|i| i.id == "img")
        .unwrap();
    assert_eq!(item.href, "images/plate.jpg");
    assert_eq!(item.media_type, "image/jpeg", "converted images are JPEG");
}

#[test]
fn percent_encoded_hrefs_are_matched_and_re_encoded() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="img" href="images/a%20plate.png" media-type="image/png"/>
  </manifest>
  <spine/>
</package>
"#);

    let map = rename_map(&[("images/a plate.png", "images/a plate.jpg")]);
    assert_eq!(
        update_opf(&doc, &Renames::new(book(), book(), &map)).unwrap(),
        1
    );

    let item = &manifest_items(&doc).unwrap()[0];
    assert_eq!(item.href, "images/a%20plate.jpg");
    assert_eq!(item.decoded_href(), "images/a plate.jpg");
}

#[test]
fn fonts_are_removed_from_the_manifest() {
    let doc = opf(MIXED_MANIFEST);
    let fonts = vec![
        Path::new("/book/fonts/body.otf").to_path_buf(),
        Path::new("/book/fonts/legacy.ttf").to_path_buf(),
    ];

    assert_eq!(update_opf_remove_fonts(&doc, &fonts).unwrap(), 2);

    let ids: Vec<String> = manifest_items(&doc)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert!(!ids.contains(&"fnt".to_string()));
    assert!(!ids.contains(&"fnt2".to_string()));
    assert!(ids.contains(&"ch1".to_string()), "content must survive");
}

#[test]
fn a_generated_cover_is_added_to_the_manifest() {
    let doc = opf(MIXED_MANIFEST);
    add_image_to_opf(&doc, "images/cover_generated.jpg", "cover-generated").unwrap();

    let item = manifest_items(&doc)
        .unwrap()
        .into_iter()
        .find(|i| i.id == "cover-generated")
        .expect("the new item should be in the manifest");
    assert_eq!(item.href, "images/cover_generated.jpg");
    assert_eq!(item.media_type, "image/jpeg");
}

#[test]
fn xhtml_image_references_follow_renames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chapter.xhtml");
    fs::write(
        &path,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body>
<img src="../images/plate.png" alt="a"/>
<div style="background-image: url('../images/plate.png')">x</div>
</body>
</html>
"#,
    )
    .unwrap();

    let map = rename_map(&[("images/plate.png", "images/plate.jpg")]);
    assert_eq!(
        update_xhtml_references(&path, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        2
    );

    let out = fs::read_to_string(&path).unwrap();
    assert!(out.contains(r#"src="../images/plate.jpg""#), "{out}");
    assert!(out.contains("url('../images/plate.jpg')"), "{out}");
    assert!(!out.contains("plate.png"), "{out}");
}

#[test]
fn xhtml_untouched_by_an_empty_rename_map() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chapter.xhtml");
    let original = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><p>Prose.</p></body></html>
"#;
    fs::write(&path, original).unwrap();

    assert_eq!(
        update_xhtml_references(
            &path,
            &Renames::new(dir.path(), dir.path(), &BTreeMap::new())
        )
        .unwrap(),
        0
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn css_url_references_follow_renames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.css");
    fs::write(
        &path,
        "body { background: url(\"images/plate.png\") no-repeat; }\n.x { background: url(images/other.gif); }\n",
    )
    .unwrap();

    let map = rename_map(&[
        ("images/plate.png", "images/plate.jpg"),
        ("images/other.gif", "images/other.jpg"),
    ]);
    assert_eq!(
        update_css_references(&path, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        1
    );

    let out = fs::read_to_string(&path).unwrap();
    assert!(out.contains(r#"url("images/plate.jpg")"#), "{out}");
    assert!(out.contains("url(images/other.jpg)"), "{out}");
}

fn put(root: &Path, name: &str, content: impl AsRef<[u8]>) -> std::path::PathBuf {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    path
}

fn chapter_with(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>
"#
    )
}

/// Images that share a filename in different directories can be renamed
/// differently, so each reference follows the file it actually names.
#[test]
fn references_follow_the_file_they_name_not_its_filename() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        chapter_with(r#"<img src="../a/pic.png" alt=""/><img src="../b/pic.png" alt=""/>"#),
    );
    let css = put(
        dir.path(),
        "styles/main.css",
        ".x { background: url(../b/pic.png) }",
    );
    let map = rename_map(&[("a/pic.png", "a/pic.jpg"), ("b/pic.png", "b/pic-2.jpg")]);

    assert_eq!(
        update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        2
    );
    assert_eq!(
        update_css_references(&css, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        1
    );

    let out = fs::read_to_string(&chapter).unwrap();
    assert!(out.contains(r#"src="../a/pic.jpg""#), "{out}");
    assert!(out.contains(r#"src="../b/pic-2.jpg""#), "{out}");
    let out = fs::read_to_string(&css).unwrap();
    assert!(out.contains("url(../b/pic-2.jpg)"), "{out}");
}

/// A path that leads to a file still on disk names something that was not
/// renamed, even if its filename matches something that was.
#[test]
fn a_reference_to_a_file_that_kept_its_name_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "b/pic.png", "an image nobody renamed");
    let chapter = put(
        dir.path(),
        "chapter.xhtml",
        chapter_with(r#"<img src="b/pic.png" alt=""/>"#),
    );
    let map = rename_map(&[("a/pic.png", "a/pic.jpg")]);

    assert_eq!(
        update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        0
    );
}

/// A path that leads nowhere falls back to its filename, but only when that is
/// unambiguous — not when two images of that name were renamed differently.
#[test]
fn an_ambiguous_filename_is_not_guessed_at() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "chapter.xhtml",
        chapter_with(r#"<img src="elsewhere/pic.png" alt=""/>"#),
    );
    let map = rename_map(&[("a/pic.png", "a/pic.jpg"), ("b/pic.png", "b/pic-2.jpg")]);
    assert_eq!(
        update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        0
    );

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest><item id="img" href="elsewhere/pic.png" media-type="image/png"/></manifest>
  <spine/>
</package>
"#);
    assert_eq!(
        update_opf(&doc, &Renames::new(book(), book(), &map)).unwrap(),
        0
    );
}

#[test]
fn links_to_other_sites_and_data_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "chapter.xhtml",
        chapter_with(
            r#"<img src="http://example.com/images/plate.png" alt=""/><img src="data:image/png;base64,AAAA" alt=""/>"#,
        ),
    );
    let map = rename_map(&[("images/plate.png", "images/plate.jpg")]);

    assert_eq!(
        update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        0
    );
}

/// Only the filename changes: one that was percent-encoded stays encoded, and
/// one written plainly stays plain.
#[test]
fn a_renamed_filename_is_written_the_way_the_reference_wrote_it() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "chapter.xhtml",
        chapter_with(
            r#"<img src="images/a%20plate.png" alt=""/><img src="images/b plate.png" alt=""/>"#,
        ),
    );
    let map = rename_map(&[
        ("images/a plate.png", "images/a plate.jpg"),
        ("images/b plate.png", "images/b plate.jpg"),
    ]);

    assert_eq!(
        update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        2
    );

    let out = fs::read_to_string(&chapter).unwrap();
    assert!(out.contains(r#"src="images/a%20plate.jpg""#), "{out}");
    assert!(out.contains(r#"src="images/b plate.jpg""#), "{out}");
}

/// A stylesheet is rewritten in place: a renamed url keeps its quotes, and
/// every other url is left exactly as it was. Unquoting a url with a space in
/// it would break the rule it sits in.
#[test]
fn css_urls_keep_their_quotes_and_unrelated_ones_are_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let css = put(
        dir.path(),
        "main.css",
        "@font-face { src: url(\"fonts/My Font.otf\"); }\n.x { background: url('images/plate.png'); }\n",
    );
    let map = rename_map(&[("images/plate.png", "images/plate.jpg")]);

    assert_eq!(
        update_css_references(&css, &Renames::new(dir.path(), dir.path(), &map)).unwrap(),
        1
    );

    let out = fs::read_to_string(&css).unwrap();
    assert!(out.contains(r#"url("fonts/My Font.otf")"#), "{out}");
    assert!(out.contains("url('images/plate.jpg')"), "{out}");
}

/// CSS as a browser reads it: `URL(` in capitals, a `)` inside a quoted url,
/// the strings `image-set()` names images with, and padding inside the
/// parentheses. A `//host` url is another site's, and a comment is not CSS.
#[test]
fn css_urls_are_found_however_they_are_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = put(
        dir.path(),
        "style.css",
        ".a { background: URL(images/upper.png) }\n\
         .b { background: url(\"images/paren(1).png\") }\n\
         .c { background-image: image-set(\"images/set.png\" 1x, url(images/set2.png) 2x) }\n\
         .d { background-image: -webkit-image-set(url( 'images/pad.png' ) 1x) }\n\
         .e { background: url(//cdn.example.com/images/plate.png) }\n\
         /* url(images/commented.png) */\n",
    );
    let map = rename_map(&[
        ("images/upper.png", "images/upper.jpg"),
        ("images/paren(1).png", "images/paren(1).jpg"),
        ("images/set.png", "images/set.jpg"),
        ("images/set2.png", "images/set2.jpg"),
        ("images/pad.png", "images/pad.jpg"),
        ("images/plate.png", "images/plate.jpg"),
        ("images/commented.png", "images/commented.jpg"),
    ]);

    update_css_references(&path, &Renames::new(dir.path(), dir.path(), &map)).unwrap();

    let out = fs::read_to_string(&path).unwrap();
    for rewritten in [
        "URL(images/upper.jpg)",
        r#"url("images/paren(1).jpg")"#,
        r#"image-set("images/set.jpg" 1x, url(images/set2.jpg) 2x)"#,
        "url( 'images/pad.jpg' )",
        "url(//cdn.example.com/images/plate.png)",
        "/* url(images/commented.png) */",
    ] {
        assert!(out.contains(rewritten), "{rewritten}:\n{out}");
    }
}

/// What an entity stands for is part of a `<style>`'s CSS, though it is not
/// text there. Read as if it were not there, `url(&cdn;cover.png)` named the
/// book's own `cover.png`, and was rewritten with the entity left behind
/// outside the url.
#[test]
fn a_url_with_an_entity_in_it_is_left_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "chapter.xhtml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html [<!ENTITY cdn "https://cdn.example/">]>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title><style type="text/css">.remote { background: url(&cdn;cover.png) }</style></head>
<body><p><img src="cover.png" alt=""/></p></body></html>
"#,
    );
    let map = rename_map(&[("cover.png", "cover.jpg")]);

    update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap();

    let out = fs::read_to_string(&chapter).unwrap();
    assert!(
        out.contains(".remote { background: url(&cdn;cover.png) }"),
        "{out}"
    );
    assert!(out.contains(r#"<img src="cover.jpg""#), "{out}");
}

/// A srcset is split as the HTML standard splits it: a URL runs to the first
/// blank, commas and all, and only a comma after it, outside parentheses,
/// ends a candidate. Split at every comma, a remote image whose URL held one
/// had the tail of its path taken for an image in the book, and a local image
/// with a comma in its name was not found.
#[test]
fn a_srcset_is_split_as_the_html_standard_splits_it() {
    let dir = tempfile::tempdir().unwrap();
    let srcsets = [
        (
            "https://cdn.example/path,cover.png 2x",
            "https://cdn.example/path,cover.png 2x",
        ),
        (
            "../images/a,b.png 1x,../images/cover.png 2x",
            "../images/a,b.jpg 1x,../images/cover.jpg 2x",
        ),
        (
            "data:image/png;base64,AAAA 1x, ../images/cover.png 2x",
            "data:image/png;base64,AAAA 1x, ../images/cover.jpg 2x",
        ),
        (
            "../images/cover.png,&#10;../images/a,b.png 640w (max-width: 9em, x) ,../images/cover.png",
            "../images/cover.jpg,\n../images/a,b.jpg 640w (max-width: 9em, x) ,../images/cover.jpg",
        ),
    ];
    let images: String = srcsets
        .iter()
        .map(|(srcset, _)| {
            format!(r#"<p><img src="../images/cover.png" srcset="{srcset}" alt=""/></p>"#)
        })
        .collect();
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body>{images}</body></html>
"#
        ),
    );
    let map = rename_map(&[
        ("images/cover.png", "images/cover.jpg"),
        ("images/a,b.png", "images/a,b.jpg"),
    ]);

    update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap();

    let out = fs::read_to_string(&chapter).unwrap();
    let doc = xml::parse_strict(out.as_bytes()).expect("the chapter should stay well-formed");
    let written: Vec<String> = xml::find_nodes(&doc, "//*[local-name()='img']")
        .unwrap()
        .iter()
        .map(|image| image.get_attribute("srcset").unwrap_or_default())
        .collect();
    let expected: Vec<&str> = srcsets.iter().map(|(_, after)| *after).collect();
    assert_eq!(written, expected, "{out}");
}

/// A `<style>` element's text and CDATA sections are one stylesheet, and a
/// url can start in one and end in the next. Read one at a time, neither held
/// a url.
#[test]
fn a_url_split_across_a_style_elements_cdata_follows_its_image() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title><style type="text/css">.a { background: url(<![CDATA[../images/a.png]]>) }</style><style type="text/css">
/*<![CDATA[*/
.b { background: url(../images/b.png) }
/*]]>*/
</style></head><body><p>x</p></body></html>
"#,
    );
    let map = rename_map(&[
        ("images/a.png", "images/a.jpg"),
        ("images/b.png", "images/b.jpg"),
    ]);

    update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap();

    let out = fs::read_to_string(&chapter).unwrap();
    assert!(
        out.contains(".a { background: url(../images/a.jpg) }"),
        "{out}"
    );
    assert!(
        out.contains("/*<![CDATA[*/\n.b { background: url(../images/b.jpg) }\n/*]]>*/"),
        "{out}"
    );
    xml::parse_strict(out.as_bytes()).expect("the chapter should stay well-formed");
}

/// An image is named by more than `<img src>`: by `srcset`, a link to the
/// full size, a video's poster, an object's data, a page's background, SVG 2's
/// plain `href` beside `xlink:href`, and `url()` in a `<style>` element as
/// much as in a `style` attribute.
#[test]
fn every_attribute_that_names_an_image_follows_it() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title><style type="text/css">.banner { background: url(../images/banner.png) }</style></head>
<body background="../images/paper.png">
<p><img src="../images/a.png" srcset="../images/a.png 1x, ../images/big.png 2x" alt=""/></p>
<p><a href="../images/a.png">full size</a></p>
<video poster="../images/poster.png"/>
<object data="../images/object.png" type="image/png"/>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image href="../images/svg.png" xlink:href="../images/svg.png"/><style>.x { fill: url(../images/fill.png) }</style></svg>
<p style="background: URL(../images/upper.png)">x</p>
<p><img src=" ../images/padded.png " alt=""/><img src="//cdn.example.com/images/a.png" alt=""/></p>
</body></html>
"#,
    );
    let map = rename_map(&[
        ("images/banner.png", "images/banner.jpg"),
        ("images/paper.png", "images/paper.jpg"),
        ("images/a.png", "images/a.jpg"),
        ("images/big.png", "images/big.jpg"),
        ("images/poster.png", "images/poster.jpg"),
        ("images/object.png", "images/object.jpg"),
        ("images/svg.png", "images/svg.jpg"),
        ("images/fill.png", "images/fill.jpg"),
        ("images/upper.png", "images/upper.jpg"),
        ("images/padded.png", "images/padded.jpg"),
    ]);

    update_xhtml_references(&chapter, &Renames::new(dir.path(), dir.path(), &map)).unwrap();

    let out = fs::read_to_string(&chapter).unwrap();
    for gone in [
        "banner.png",
        "paper.png",
        "../images/a.png",
        "big.png",
        "poster.png",
        "object.png",
        "svg.png",
        "fill.png",
        "upper.png",
        "padded.png",
    ] {
        assert!(!out.contains(gone), "{gone} is still named:\n{out}");
    }
    assert!(
        out.contains(r#"srcset="../images/a.jpg 1x, ../images/big.jpg 2x""#),
        "{out}"
    );
    assert!(out.contains("//cdn.example.com/images/a.png"), "{out}");
    xml::parse_strict(out.as_bytes()).expect("the chapter should stay well-formed");
}

#[test]
fn svg_wrapped_covers_become_plain_images() {
    let dir = tempfile::tempdir().unwrap();
    let cover = dir.path().join("cover.xhtml");
    fs::write(&cover, r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body>
<div>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 600 800">
<image width="600" height="800" xlink:href="images/cover.jpg"/>
</svg>
</div>
</body>
</html>
"#).unwrap();

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest><item id="cover" href="cover.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="cover"/></spine>
</package>
"#);

    assert_eq!(fix_svg_covers(dir.path(), dir.path(), &doc).unwrap(), 1);

    let out = fs::read_to_string(&cover).unwrap();
    assert!(out.contains(r#"src="images/cover.jpg""#), "{out}");
    assert!(out.contains(r#"alt="Cover""#), "{out}");
    assert!(
        !out.contains("<svg"),
        "the svg wrapper should be gone:\n{out}"
    );
}

/// An SVG holding several images is an illustration, not a cover wrapper.
#[test]
fn multi_image_svgs_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let page = dir.path().join("page.xhtml");
    let original = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
<image xlink:href="a.jpg"/>
<image xlink:href="b.jpg"/>
</svg>
</body>
</html>
"#;
    fs::write(&page, original).unwrap();

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest><item id="p" href="page.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="p"/></spine>
</package>
"#);

    assert_eq!(fix_svg_covers(dir.path(), dir.path(), &doc).unwrap(), 0);
    assert_eq!(fs::read_to_string(&page).unwrap(), original);
}

/// An SVG that draws more than its image, a label or a line, is an
/// illustration, and so is one that draws its image turned; an `<img>` would
/// lose the rest. One that adds only a title or a description is still a
/// wrapper.
#[test]
fn only_an_svg_that_just_shows_its_image_is_unwrapped() {
    let dir = tempfile::tempdir().unwrap();
    let svg = |inside: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 600 800">
{inside}
</svg>
</body>
</html>
"#
        )
    };
    let cover = put(
        dir.path(),
        "cover.xhtml",
        svg(r#"<title>Cover</title><desc>The front cover</desc>
<image width="600" height="800" xlink:href="images/cover.jpg"/>"#),
    );
    let labelled = svg(
        r#"<image width="600" height="800" xlink:href="images/map.png"/>
<path d="M 10 10 L 590 790" stroke="black"/>
<text x="300" y="400">ESSENTIAL MAP LABEL</text>"#,
    );
    let map = put(dir.path(), "map.xhtml", &labelled);
    let turned = svg(
        r#"<image width="600" height="800" transform="rotate(90 300 400)" xlink:href="images/plate.png"/>"#,
    );
    let plate = put(dir.path(), "plate.xhtml", &turned);

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="cover" href="cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="map" href="map.xhtml" media-type="application/xhtml+xml"/>
    <item id="plate" href="plate.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="cover"/><itemref idref="map"/><itemref idref="plate"/></spine>
</package>
"#);

    assert_eq!(fix_svg_covers(dir.path(), dir.path(), &doc).unwrap(), 1);
    assert!(!fs::read_to_string(&cover).unwrap().contains("<svg"));
    assert_eq!(fs::read_to_string(&map).unwrap(), labelled);
    assert_eq!(fs::read_to_string(&plate).unwrap(), turned);
}

/// An SVG inside another is part of that illustration; an `<img>` in its
/// place would not show inside an SVG at all.
#[test]
fn an_svg_inside_another_is_left_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let original = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<body>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 600 800">
<svg x="100" y="100" width="400" height="600"><image width="400" height="600" xlink:href="images/inset.jpg"/></svg>
<text x="300" y="50">Caption</text>
</svg>
</body>
</html>
"#;
    let page = put(dir.path(), "page.xhtml", original);

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest><item id="p" href="page.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="p"/></spine>
</package>
"#);

    assert_eq!(fix_svg_covers(dir.path(), dir.path(), &doc).unwrap(), 0);
    assert_eq!(fs::read_to_string(&page).unwrap(), original);
}

fn write_chapter(path: &Path, title: &str, heading: &str) {
    fs::write(
        path,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>{title}</title></head>
<body><h1>{heading}</h1><p>Text.</p></body>
</html>
"#
        ),
    )
    .unwrap();
}

const TWO_CHAPTER_OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="ch1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="c2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>
"#;

#[test]
fn a_missing_toc_is_generated_from_the_spine() {
    let dir = tempfile::tempdir().unwrap();
    write_chapter(&dir.path().join("c1.xhtml"), "First Chapter", "One");
    write_chapter(&dir.path().join("c2.xhtml"), "Second Chapter", "Two");

    let doc = opf(TWO_CHAPTER_OPF);
    assert_eq!(
        fix_toc(dir.path(), dir.path(), &doc).unwrap(),
        TocOutcome::Generated(2)
    );

    let ncx = fs::read_to_string(dir.path().join("toc.ncx")).unwrap();
    assert!(ncx.contains("First Chapter"), "{ncx}");
    assert!(ncx.contains("Second Chapter"), "{ncx}");
    assert!(ncx.contains(r#"src="c1.xhtml""#), "{ncx}");

    // The generated NCX must itself be well-formed and declared in the OPF.
    xml::parse_strict(ncx.as_bytes()).expect("generated NCX should parse");
    let items = manifest_items(&doc).unwrap();
    assert!(items
        .iter()
        .any(|i| i.media_type == "application/x-dtbncx+xml"));
}

#[test]
fn a_healthy_toc_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    write_chapter(&dir.path().join("c1.xhtml"), "First", "One");
    write_chapter(&dir.path().join("c2.xhtml"), "Second", "Two");
    fs::write(
        dir.path().join("toc.ncx"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n1" playOrder="1">
      <navLabel><text>First</text></navLabel>
      <content src="c1.xhtml"/>
    </navPoint>
  </navMap>
</ncx>
"#,
    )
    .unwrap();

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="ch1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx"><itemref idref="ch1"/><itemref idref="ch2"/></spine>
</package>
"#);

    assert_eq!(
        fix_toc(dir.path(), dir.path(), &doc).unwrap(),
        TocOutcome::Valid
    );
}

/// The reference implementation detected broken references and then did
/// nothing about them — its fix-up function was an empty stub. Regenerating is
/// what it meant to do.
#[test]
fn a_toc_pointing_at_missing_files_is_regenerated() {
    let dir = tempfile::tempdir().unwrap();
    write_chapter(&dir.path().join("c1.xhtml"), "Real Chapter", "One");
    write_chapter(&dir.path().join("c2.xhtml"), "Other Chapter", "Two");
    fs::write(
        dir.path().join("toc.ncx"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="n1" playOrder="1">
      <navLabel><text>Ghost</text></navLabel>
      <content src="deleted.xhtml"/>
    </navPoint>
  </navMap>
</ncx>
"#,
    )
    .unwrap();

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="ch1" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx"><itemref idref="ch1"/><itemref idref="ch2"/></spine>
</package>
"#);

    assert_eq!(
        fix_toc(dir.path(), dir.path(), &doc).unwrap(),
        TocOutcome::Generated(2)
    );

    let ncx = fs::read_to_string(dir.path().join("toc.ncx")).unwrap();
    assert!(!ncx.contains("deleted.xhtml"), "{ncx}");
    assert!(ncx.contains("Real Chapter"), "{ncx}");
}

#[test]
fn chapters_without_titles_fall_back_to_headings_then_position() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("c1.xhtml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Heading Only</h1></body></html>
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("c2.xhtml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><p>No title, no heading.</p></body></html>
"#,
    )
    .unwrap();

    let doc = opf(TWO_CHAPTER_OPF);
    assert_eq!(
        fix_toc(dir.path(), dir.path(), &doc).unwrap(),
        TocOutcome::Generated(2)
    );

    let ncx = fs::read_to_string(dir.path().join("toc.ncx")).unwrap();
    assert!(ncx.contains("Heading Only"), "{ncx}");
    assert!(ncx.contains("Chapter 2"), "{ncx}");
}

#[test]
fn titles_needing_escapes_produce_valid_ncx() {
    let dir = tempfile::tempdir().unwrap();
    write_chapter(&dir.path().join("c1.xhtml"), "Cause &amp; Effect", "One");
    write_chapter(&dir.path().join("c2.xhtml"), "A &lt;Tag&gt;", "Two");

    let doc = opf(TWO_CHAPTER_OPF);
    fix_toc(dir.path(), dir.path(), &doc).unwrap();

    let ncx = fs::read_to_string(dir.path().join("toc.ncx")).unwrap();
    xml::parse_strict(ncx.as_bytes()).expect("NCX with escaped titles should parse");
    assert!(ncx.contains("Cause &amp; Effect"), "{ncx}");
}

/// Chapter links in the OPF are relative to the OPF; in a generated NCX they
/// have to be relative to the NCX, which need not sit beside it.
#[test]
fn a_regenerated_ncx_links_its_chapters_from_where_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let ops = dir.path().join("OPS");
    fs::create_dir_all(ops.join("Navigation")).unwrap();
    fs::create_dir_all(ops.join("Text")).unwrap();
    write_chapter(&ops.join("chapter.xhtml"), "First", "One");
    write_chapter(&ops.join("Text").join("Part Two.xhtml"), "Second", "Two");
    fs::write(ops.join("Navigation").join("toc.ncx"), "not an NCX").unwrap();

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="ch1" href="chapter.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="Text/Part%20Two.xhtml" media-type="application/xhtml+xml"/>
    <item id="toc" href="Navigation/toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="toc">
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>
"#);

    assert_eq!(
        fix_toc(dir.path(), &ops, &doc).unwrap(),
        TocOutcome::Generated(2)
    );

    let ncx = fs::read_to_string(ops.join("Navigation").join("toc.ncx")).unwrap();
    assert!(ncx.contains(r#"src="../chapter.xhtml""#), "{ncx}");
    assert!(ncx.contains(r#"src="../Text/Part%20Two.xhtml""#), "{ncx}");
    // What was written is a table of contents the same check now accepts.
    assert_eq!(fix_toc(dir.path(), &ops, &doc).unwrap(), TocOutcome::Valid);
}

/// A chapter outside the book has no place in its table of contents, and a
/// spine with nothing else gives it nothing to list.
#[test]
fn a_toc_lists_no_chapter_outside_the_book() {
    let dir = tempfile::tempdir().unwrap();
    let book = dir.path().join("book");
    fs::create_dir(&book).unwrap();
    write_chapter(&book.join("c1.xhtml"), "Inside", "One");
    write_chapter(&dir.path().join("c2.xhtml"), "Outside", "Two");

    let doc = opf(&TWO_CHAPTER_OPF.replace("c2.xhtml", "../c2.xhtml"));
    assert_eq!(
        fix_toc(&book, &book, &doc).unwrap(),
        TocOutcome::Generated(1)
    );
    let ncx = fs::read_to_string(book.join("toc.ncx")).unwrap();
    assert!(!ncx.contains("c2.xhtml"), "{ncx}");

    let elsewhere = tempfile::tempdir().unwrap();
    let doc = opf(&TWO_CHAPTER_OPF
        .replace("c1.xhtml", "../c1.xhtml")
        .replace("c2.xhtml", "../c2.xhtml"));
    assert!(matches!(
        fix_toc(elsewhere.path(), elsewhere.path(), &doc).unwrap(),
        TocOutcome::Skipped(_)
    ));
    assert!(!elsewhere.path().join("toc.ncx").exists());
}

/// IDs are unique across the whole package document, so a generated NCX
/// takes one nothing else has, and the spine points at it by that.
#[test]
fn a_generated_ncx_takes_an_id_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    write_chapter(&dir.path().join("c1.xhtml"), "First", "One");
    write_chapter(&dir.path().join("c2.xhtml"), "Second", "Two");

    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="ncx-2">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="ncx-2">urn:uuid:demo</dc:identifier>
    <dc:title>T</dc:title>
  </metadata>
  <manifest>
    <item id="ncx" href="c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="c2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ncx"/>
    <itemref idref="ch2"/>
  </spine>
</package>
"#);

    assert_eq!(
        fix_toc(dir.path(), dir.path(), &doc).unwrap(),
        TocOutcome::Generated(2)
    );

    let ids: Vec<String> = xml::find_nodes(&doc, "//*[@id]")
        .unwrap()
        .iter()
        .map(|node| node.get_attribute("id").unwrap())
        .collect();
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "{ids:?}");

    let ncx = manifest_items(&doc)
        .unwrap()
        .into_iter()
        .find(|item| item.media_type == "application/x-dtbncx+xml")
        .expect("the NCX is declared");
    let spine = xml::find_first(&doc, "//*[local-name()='spine']")
        .unwrap()
        .unwrap();
    assert_eq!(spine.get_attribute("toc"), Some(ncx.id));
    assert_eq!(
        spine_hrefs(&doc).unwrap()[0],
        ("ncx".to_string(), "c1.xhtml".to_string())
    );
}

#[test]
fn an_empty_spine_is_reported_not_guessed_at() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest/><spine/>
</package>
"#);

    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        fix_toc(dir.path(), dir.path(), &doc).unwrap(),
        TocOutcome::Skipped(_)
    ));
}

// ---------------------------------------------------- Light Novel reshaping

fn split_spread() -> BTreeMap<String, Vec<String>> {
    BTreeMap::from([(
        "images/spread_part1.jpg".to_string(),
        vec![
            "images/spread_part1.jpg".to_string(),
            "images/spread_part2.jpg".to_string(),
        ],
    )])
}

/// The first page keeps its place; each further page follows it, shown the
/// same way but without an id, which has to stay unique. The spread's size no
/// longer describes either page.
#[test]
fn a_split_image_is_followed_by_its_other_pages() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body>
<p><img id="spread" class="plate" epub:type="illustration" src="../images/spread_part1.jpg" alt="Both pages" width="1000" height="400"/></p>
<p><img src="../images/other.jpg" alt="" width="10" height="20"/></p>
</body></html>
"#,
    );

    assert_eq!(
        show_reshaped_pages(
            &chapter,
            &ReshapedPages::new(dir.path(), dir.path(), &split_spread())
        )
        .unwrap(),
        1
    );

    let out = fs::read_to_string(&chapter).unwrap();
    let first = out.find("spread_part1.jpg").expect("first page");
    let second = out.find("spread_part2.jpg").expect("second page");
    assert!(first < second, "{out}");
    assert_eq!(out.matches(r#"class="plate""#).count(), 2, "{out}");
    assert_eq!(
        out.matches(r#"epub:type="illustration""#).count(),
        2,
        "{out}"
    );
    assert_eq!(out.matches(r#"id="spread""#).count(), 1, "{out}");
    assert!(!out.contains(r#"width="1000""#), "{out}");
    assert!(
        out.contains(r#"width="10" height="20""#),
        "an unrelated image keeps its size: {out}"
    );
}

/// Each page of a split image is shown by its `src`. A `srcset` or `sizes`,
/// or the `<source>`s of a `<picture>`, would show the one image they name on
/// every page instead: the first page, or the whole spread.
#[test]
fn a_split_images_pages_are_shown_by_src_alone() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
<p><img src="../images/spread_part1.jpg" srcset="../images/spread_part1.jpg 1x, ../images/spread-hd.jpg 2x" sizes="100vw" alt=""/></p>
<p><picture><source srcset="../images/spread.webp" type="image/webp"/><img src="../images/spread_part1.jpg" alt=""/></picture></p>
<p><img src="../images/other.jpg" srcset="../images/other.jpg 1x" sizes="50vw" alt=""/></p>
</body></html>
"#,
    );

    assert_eq!(
        show_reshaped_pages(
            &chapter,
            &ReshapedPages::new(dir.path(), dir.path(), &split_spread())
        )
        .unwrap(),
        2
    );

    let out = fs::read_to_string(&chapter).unwrap();
    assert_eq!(out.matches("spread_part2.jpg").count(), 2, "{out}");
    for gone in ["spread-hd.jpg", "100vw", "<source", "spread.webp"] {
        assert!(!out.contains(gone), "{gone}: {out}");
    }
    assert!(
        out.contains(r#"srcset="../images/other.jpg 1x" sizes="50vw""#),
        "an unrelated image keeps its srcset: {out}"
    );
    xml::parse_strict(out.as_bytes()).expect("the chapter should stay well-formed");
}

/// A `<picture>` is cleared of its sources once, not once for each image in
/// it: looked through again for every one, a picture of 4,000 split images
/// took thirteen seconds, four times as long for twice as many.
#[test]
fn a_picture_of_many_split_images_is_cleared_once() {
    let dir = tempfile::tempdir().unwrap();
    let images = 8_000;
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><picture><source srcset="../images/spread.webp"/>{}</picture></body></html>
"#,
            r#"<img src="../images/spread_part1.jpg" alt=""/>"#.repeat(images)
        ),
    );
    let root = dir.path().to_path_buf();

    let changed = common::finishes_within(Duration::from_secs(20), move || {
        show_reshaped_pages(&chapter, &ReshapedPages::new(&root, &root, &split_spread())).unwrap()
    });

    assert_eq!(changed, images);
}

/// An SVG wrapper's viewBox is sized to the old shape, so it would squash the
/// new pages into it. It gives way to a plain image per page.
#[test]
fn an_svg_wrapper_around_a_reshaped_image_gives_way_to_plain_images() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><div class="illust">
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 1000 400"><image width="1000" height="400" xlink:href="../images/spread_part1.jpg"/></svg>
</div></body></html>
"#,
    );

    assert_eq!(
        show_reshaped_pages(
            &chapter,
            &ReshapedPages::new(dir.path(), dir.path(), &split_spread())
        )
        .unwrap(),
        1
    );

    let out = fs::read_to_string(&chapter).unwrap();
    assert!(!out.contains("<svg"), "{out}");
    assert!(
        out.contains(r#"<img src="../images/spread_part1.jpg""#),
        "{out}"
    );
    assert!(
        out.contains(r#"<img src="../images/spread_part2.jpg""#),
        "{out}"
    );
    xml::parse_strict(out.as_bytes()).expect("the chapter should stay well-formed");
}

/// An illustration drawn around a split image keeps everything it draws, and
/// the image's further pages follow it.
#[test]
fn an_illustration_around_a_split_image_is_kept_and_followed_by_its_pages() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "text/chapter.xhtml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><div class="illust">
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 1000 400"><image width="1000" height="400" xlink:href="../images/spread_part1.jpg"/><text x="500" y="200">ESSENTIAL MAP LABEL</text></svg>
</div></body></html>
"#,
    );

    assert_eq!(
        show_reshaped_pages(
            &chapter,
            &ReshapedPages::new(dir.path(), dir.path(), &split_spread())
        )
        .unwrap(),
        1
    );

    let out = fs::read_to_string(&chapter).unwrap();
    assert!(out.contains("ESSENTIAL MAP LABEL"), "{out}");
    assert!(
        out.contains(r#"xlink:href="../images/spread_part1.jpg""#),
        "{out}"
    );
    let svg_end = out.find("</svg>").expect("the illustration is kept");
    let second = out
        .find(r#"<img src="../images/spread_part2.jpg""#)
        .expect("the second page is shown");
    assert!(second > svg_end, "{out}");
    xml::parse_strict(out.as_bytes()).expect("the chapter should stay well-formed");
}

/// Each further page is a copy of the first page's image. A prefix the image
/// declared for itself has to be declared on each copy too, or the chapter
/// stops being namespace-well-formed. An `xml:id` is an id like any other and
/// stays with the first. And the copy is the same every run: its attributes
/// come in the order the original has them.
#[test]
fn copied_page_images_declare_what_they_use_and_come_out_the_same_every_time() {
    let chapter_text = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
<p><img xmlns:epub="http://www.idpf.org/2007/ops" xml:id="x1" id="spread" class="plate" epub:type="illustration" src="../images/spread_part1.jpg" alt="Both pages" title="A spread"/></p>
</body></html>
"#;

    let mut outputs = Vec::new();
    for _ in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let chapter = put(dir.path(), "text/chapter.xhtml", chapter_text);
        show_reshaped_pages(
            &chapter,
            &ReshapedPages::new(dir.path(), dir.path(), &split_spread()),
        )
        .unwrap();
        outputs.push(fs::read_to_string(&chapter).unwrap());
    }
    let out = &outputs[0];
    assert!(outputs.iter().all(|other| other == out), "{outputs:#?}");

    assert_eq!(out.matches("xml:id=").count(), 1, "{out}");
    let doc = xml::parse_strict(out.as_bytes()).unwrap();
    let images = xml::find_nodes(&doc, "//*[local-name()='img']").unwrap();
    assert_eq!(images.len(), 2, "{out}");
    assert_eq!(
        images[1].get_attribute_ns("type", "http://www.idpf.org/2007/ops"),
        Some("illustration".to_string()),
        "the copy's epub:type is not in the epub namespace:\n{out}"
    );
    let copy = &out[out.rfind("<img").unwrap()..];
    let copy = &copy[..copy.find("/>").unwrap()];
    let order: Vec<usize> = ["class=", "epub:type=", "src=", "alt=", "title="]
        .iter()
        .map(|name| {
            copy.find(name)
                .unwrap_or_else(|| panic!("{name} missing: {copy}"))
        })
        .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{copy}");
}

/// A chapter the manifest lists twice, under two spellings, is one file, and
/// is processed once.
#[test]
fn a_file_listed_twice_is_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="a" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="b" href="Text/./ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c" href="Images/a.png" media-type="image/png"/>
    <item id="d" href="Text/../Images/a.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="a"/></spine>
</package>
"#);

    let content = find_content_files(dir.path(), dir.path(), &doc).unwrap();
    assert_eq!(content.xhtml.len(), 1, "{:?}", content.xhtml);
    assert_eq!(content.images.len(), 1, "{:?}", content.images);
}

/// The manifest follows an image by the file its href names. One whose file
/// is missing named something else, and re-pointing it at another folder's
/// image of the same name made two items share one file.
#[test]
fn a_manifest_item_for_a_missing_file_is_not_repointed() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
  <manifest>
    <item id="cover" href="Images/cover.png" media-type="image/png"/>
    <item id="thumb" href="Thumbs/cover.png" media-type="image/png"/>
  </manifest>
  <spine/>
</package>
"#);
    let map = rename_map(&[("Images/cover.png", "Images/cover.jpg")]);

    assert_eq!(
        update_opf(&doc, &Renames::new(book(), book(), &map)).unwrap(),
        1
    );
    let hrefs: Vec<String> = manifest_items(&doc)
        .unwrap()
        .into_iter()
        .map(|item| item.href)
        .collect();
    assert_eq!(hrefs, ["Images/cover.jpg", "Thumbs/cover.png"]);
}

/// A rotated image is one page, but no longer the shape its size describes.
#[test]
fn a_rotated_image_loses_the_size_it_no_longer_has() {
    let dir = tempfile::tempdir().unwrap();
    let chapter = put(
        dir.path(),
        "chapter.xhtml",
        chapter_with(r#"<img src="images/plate.jpg" alt="" width="800" height="600"/>"#),
    );
    let rotated = BTreeMap::from([(
        "images/plate.jpg".to_string(),
        vec!["images/plate.jpg".to_string()],
    )]);

    assert_eq!(
        show_reshaped_pages(
            &chapter,
            &ReshapedPages::new(dir.path(), dir.path(), &rotated)
        )
        .unwrap(),
        1
    );

    let out = fs::read_to_string(&chapter).unwrap();
    assert_eq!(out.matches("<img").count(), 1, "{out}");
    assert!(!out.contains("width="), "{out}");
}

#[test]
fn split_pages_are_declared_under_ids_of_their_own() {
    let doc = opf(r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="plate-2-2">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="plate-2-2">urn:uuid:demo</dc:identifier>
    <dc:title>T</dc:title>
  </metadata>
  <manifest>
    <item id="plate" href="images/spread_part1.jpg" media-type="image/jpeg"/>
    <item id="plate-2" href="images/unrelated.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine/>
</package>
"#);

    assert_eq!(declare_reshaped_pages(&doc, &split_spread()).unwrap(), 1);

    let added = manifest_items(&doc)
        .unwrap()
        .into_iter()
        .find(|item| item.href == "images/spread_part2.jpg")
        .expect("the second page should be declared");
    assert_eq!(
        added.id, "plate-2-3",
        "plate-2 was taken in the manifest, plate-2-2 in the metadata"
    );
    assert_eq!(added.media_type, "image/jpeg");
}

/// An href leads somewhere inside the book, or nowhere. A leading slash
/// starts from the book's root, as URLs inside an EPUB container do.
#[test]
fn hrefs_resolve_inside_the_book_or_not_at_all() {
    let root = Path::new("/work");
    let opf_dir = Path::new("/work/OEBPS");
    let inside = |href: &str| resolve_href(root, opf_dir, href);

    assert_eq!(
        inside("Text/one.xhtml"),
        Some("/work/OEBPS/Text/one.xhtml".into())
    );
    assert_eq!(inside("../Images/a.png"), Some("/work/Images/a.png".into()));
    assert_eq!(inside("./Text/../b.css"), Some("/work/OEBPS/b.css".into()));
    assert_eq!(
        inside("/OEBPS/one.xhtml"),
        Some("/work/OEBPS/one.xhtml".into())
    );

    assert_eq!(inside("../../etc/passwd"), None);
    assert_eq!(inside("Text/../../../x"), None);
    assert_eq!(inside("/../x"), None);
}
