use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use epubkit_core::structure::{
    add_image_to_opf, build_rename_map, find_content_files, fix_svg_covers, fix_toc,
    manifest_items, spine_hrefs, update_css_references, update_opf, update_opf_remove_fonts,
    update_xhtml_references, TocOutcome,
};
use epubkit_core::xml;

fn opf(body: &str) -> libxml::tree::Document {
    xml::parse_strict(body.as_bytes()).expect("fixture should parse")
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
    let files = find_content_files(Path::new("/book"), &opf(MIXED_MANIFEST)).unwrap();

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
    let files = find_content_files(Path::new("/book"), &opf(MIXED_MANIFEST)).unwrap();
    assert_eq!(
        files.fonts,
        vec![
            Path::new("/book/fonts/body.otf"),
            Path::new("/book/fonts/legacy.ttf"),
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

    assert_eq!(update_opf(&doc, &map).unwrap(), 1);

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
    assert_eq!(update_opf(&doc, &map).unwrap(), 1);

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
    assert_eq!(update_xhtml_references(&path, &map).unwrap(), 2);

    let out = fs::read_to_string(&path).unwrap();
    assert!(out.contains(r#"src="../images/plate.jpg""#), "{out}");
    assert!(out.contains("url(../images/plate.jpg)"), "{out}");
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

    assert_eq!(update_xhtml_references(&path, &BTreeMap::new()).unwrap(), 0);
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
    assert_eq!(update_css_references(&path, &map).unwrap(), 1);

    let out = fs::read_to_string(&path).unwrap();
    assert!(out.contains("url(images/plate.jpg)"), "{out}");
    assert!(out.contains("url(images/other.jpg)"), "{out}");
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

    assert_eq!(fix_svg_covers(dir.path(), &doc).unwrap(), 1);

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

    assert_eq!(fix_svg_covers(dir.path(), &doc).unwrap(), 0);
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
    assert_eq!(fix_toc(dir.path(), &doc).unwrap(), TocOutcome::Generated(2));

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

    assert_eq!(fix_toc(dir.path(), &doc).unwrap(), TocOutcome::Valid);
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

    assert_eq!(fix_toc(dir.path(), &doc).unwrap(), TocOutcome::Generated(2));

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
    assert_eq!(fix_toc(dir.path(), &doc).unwrap(), TocOutcome::Generated(2));

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
    fix_toc(dir.path(), &doc).unwrap();

    let ncx = fs::read_to_string(dir.path().join("toc.ncx")).unwrap();
    xml::parse_strict(ncx.as_bytes()).expect("NCX with escaped titles should parse");
    assert!(ncx.contains("Cause &amp; Effect"), "{ncx}");
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
        fix_toc(dir.path(), &doc).unwrap(),
        TocOutcome::Skipped(_)
    ));
}
