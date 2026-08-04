//! Thin libxml2 wrapper for the well-formed XML an EPUB carries:
//! `container.xml`, `encryption.xml`, and later the OPF and NCX.
//!
//! XHTML content documents are *not* handled here — they are frequently
//! malformed and go through [`crate::html`], which layers recovery on top.

use libxml::parser::{Parser, ParserOptions};
use libxml::tree::Document;
use libxml::xpath::Context;

use crate::{Error, Result};

/// Parser options hardened for untrusted input.
///
/// EPUBs arrive from the open internet, so the parser must not be a network
/// client or an amplifier:
///
/// - `no_net` forbids fetching external DTDs and entities.
/// - libxml2's `NOENT` flag is never set (this crate does not expose it), so
///   internal entity references are left as references instead of being
///   substituted. That is what stops a "billion laughs" payload from
///   inflating in memory.
/// - `huge` stays `false`, keeping libxml2's built-in depth and expansion
///   limits in force.
///
/// `no_error`/`no_warning` only silence libxml2's chatter on stderr; parse
/// failure is still detected from the returned result.
pub(crate) fn hardened_options(recover: bool) -> ParserOptions<'static> {
    ParserOptions {
        recover,
        no_error: true,
        no_warning: true,
        no_net: true,
        huge: false,
        ..ParserOptions::default()
    }
}

/// Parse well-formed XML, refusing to guess at broken markup.
pub fn parse_strict(bytes: &[u8]) -> Result<Document> {
    Parser::default()
        .parse_string_with_options(bytes, hardened_options(false))
        .map_err(|e| Error::Xml(e.to_string()))
}

/// Evaluate an XPath expression and return the matching nodes' values for
/// `attribute`. Namespace prefixes must be registered up front via `namespaces`
/// as `(prefix, uri)` pairs.
pub fn attribute_values(
    doc: &Document,
    xpath: &str,
    attribute: &str,
    namespaces: &[(&str, &str)],
) -> Result<Vec<String>> {
    let mut context =
        Context::new(doc).map_err(|_| Error::Xml("could not create XPath context".into()))?;

    for (prefix, uri) in namespaces {
        context
            .register_namespace(prefix, uri)
            .map_err(|_| Error::Xml(format!("could not register namespace {prefix}")))?;
    }

    let nodes = context
        .findnodes(xpath, None)
        .map_err(|_| Error::Xml(format!("could not evaluate XPath: {xpath}")))?;

    Ok(nodes
        .iter()
        .filter_map(|node| node.get_attribute(attribute))
        .collect())
}
