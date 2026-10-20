//! A small element tree over quick-xml for the `JetBrains` files. Depth and
//! element count are capped; DTDs and entity declarations are ignored.

use dq_core::{DbError, DbResult};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

const MAX_DEPTH: usize = 64;
const MAX_ELEMENTS: usize = 200_000;

#[derive(Debug, Default)]
pub(super) struct Element {
    pub name: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<Element>,
}

impl Element {
    fn start(e: &BytesStart<'_>) -> Self {
        let attrs = e
            .attributes()
            .flatten()
            .map(|a| {
                let value = a.normalized_value(XmlVersion::Implicit1_0).map(std::borrow::Cow::into_owned);
                (a.key.local_name().as_ref().to_owned(), value.unwrap_or_default())
            })
            .collect();
        Self { name: e.local_name().as_ref().to_owned(), attrs, ..Self::default() }
    }

    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim()).filter(|v| !v.is_empty())
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Trimmed text of the first child called `name`, or of its `value` attribute.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        let child = self.child(name)?;
        Some(child.text.trim()).filter(|t| !t.is_empty()).or_else(|| child.attr("value"))
    }

    /// Every element called `name` below this one, in document order.
    pub fn descendants<'a>(&'a self, name: &str) -> Vec<&'a Element> {
        let mut found = Vec::new();
        let mut stack: Vec<&Element> = self.children.iter().rev().collect();
        while let Some(e) = stack.pop() {
            if e.name == name {
                found.push(e);
            }
            stack.extend(e.children.iter().rev());
        }
        found
    }
}

fn count_element(count: &mut usize, depth: usize) -> DbResult<()> {
    *count += 1;
    if depth > MAX_DEPTH || *count > MAX_ELEMENTS {
        return Err(DbError::invalid("The XML file is nested too deeply or is too large."));
    }
    Ok(())
}

/// Parses a document into its root element's children, under a synthetic root.
pub(super) fn parse(text: &str) -> DbResult<Element> {
    let invalid = || DbError::invalid("The file is not valid XML.");
    let mut reader = Reader::from_str(text);
    let mut stack = vec![Element::default()];
    let mut count = 0usize;
    loop {
        let event = reader.read_event().map_err(|_| invalid())?;
        match event {
            Event::Start(e) => {
                count_element(&mut count, stack.len())?;
                stack.push(Element::start(&e));
            }
            Event::Empty(e) => {
                count_element(&mut count, stack.len())?;
                let element = Element::start(&e);
                stack.last_mut().ok_or_else(invalid)?.children.push(element);
            }
            Event::End(_) => {
                let element = stack.pop().filter(|_| !stack.is_empty()).ok_or_else(invalid)?;
                stack.last_mut().ok_or_else(invalid)?.children.push(element);
            }
            Event::Text(t) => stack.last_mut().ok_or_else(invalid)?.text.push_str(&t.xml10_content()),
            Event::CData(t) => stack.last_mut().ok_or_else(invalid)?.text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c.to_string()),
                    _ => quick_xml::escape::resolve_predefined_entity(&r).map(str::to_owned),
                };
                stack.last_mut().ok_or_else(invalid)?.text.push_str(&resolved.unwrap_or_default());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(invalid());
    }
    Ok(stack.pop().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_attributes_text_and_entities() {
        let doc = parse(r#"<?xml version="1.0"?><a x="1 &amp; 2"><b>t &lt; u&#33;</b><c value="v"/><b>second</b></a>"#)
            .unwrap();
        let a = doc.child("a").unwrap();
        assert_eq!(a.attr("x"), Some("1 & 2"));
        assert_eq!(a.child_text("b"), Some("t < u!"));
        assert_eq!(a.child_text("c"), Some("v"));
        assert_eq!(doc.descendants("b").len(), 2);
    }

    #[test]
    fn rejects_broken_and_deep_documents() {
        assert!(parse("<a><b></a>").is_err());
        assert!(parse(&"<a>".repeat(100)).is_err());
    }
}
