//! Scratch comparison of libxml2 parse/serialize combinations for XHTML
//! repair. Kept in the tree because it is the evidence behind the backend
//! choice documented in `html::libxml_repair` — rerun it whenever that choice
//! is revisited:
//!
//! ```sh
//! cargo run -p epubkit-core --example probe -- chapter1.xhtml
//! ```

use libxml::parser::{Parser, ParserOptions};
use libxml::tree::SaveOptions;

fn options(recover: bool) -> ParserOptions<'static> {
    ParserOptions {
        recover,
        no_error: true,
        no_warning: true,
        no_net: true,
        huge: false,
        ..ParserOptions::default()
    }
}

fn save(xhtml: bool, as_xml: bool, as_html: bool, format: bool) -> SaveOptions {
    SaveOptions {
        format,
        no_declaration: false,
        no_empty_tags: false,
        no_xhtml: false,
        xhtml,
        as_xml,
        as_html,
        non_significant_whitespace: false,
    }
}

fn show(label: &str, parsed: Option<String>) {
    println!("\n======== {label} ========");
    println!("{}", parsed.unwrap_or_else(|| "<parse failed>".to_string()));
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe <file.xhtml>");
    let bytes = std::fs::read(&path).expect("read input");

    let xml = Parser::default();
    let html = Parser::default_html();

    show(
        "A: XML parser + recover, serialize XHTML",
        xml.parse_string_with_options(&bytes, options(true))
            .ok()
            .map(|d| d.to_string_with_options(save(true, true, false, false))),
    );

    show(
        "B: XML parser + recover, serialize XML",
        xml.parse_string_with_options(&bytes, options(true))
            .ok()
            .map(|d| d.to_string_with_options(save(false, true, false, false))),
    );

    show(
        "C: HTML parser + recover, serialize XML",
        html.parse_string_with_options(&bytes, options(true))
            .ok()
            .map(|d| d.to_string_with_options(save(false, true, false, false))),
    );

    show(
        "D: HTML parser + recover, serialize HTML (the Python's path)",
        html.parse_string_with_options(&bytes, options(true))
            .ok()
            .map(|d| d.to_string_with_options(save(false, false, true, true))),
    );
}
