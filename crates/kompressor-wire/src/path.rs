//! Where a text slot lives inside the parsed request body.
//!
//! Write-back cannot find its target by re-walking and matching text, for two
//! independent reasons. Compression is precisely what changed the text, so the
//! recorded bytes no longer occur; and agent traffic repeats itself — two tool
//! results in one request are routinely byte-identical — so even an unchanged
//! slot would be ambiguous. The walk therefore records an address at the moment
//! it reads a value, and `apply` resolves that address again.
//!
//! The address is a generic key/index path rather than one enum variant per
//! wire shape. The three families between them address text as
//! `messages[3].content`, `messages[3].content[2].text`,
//! `messages[3].content[2].content[0].text`, `input`, `input[7].content[0].text`
//! and `input[7].output`, and every one of those is the same traversal with
//! different steps. One resolver that cannot disagree with the recorded address
//! is worth more than a type that enumerates today's shapes.

use std::fmt;

use serde_json::Value;

/// One step of a [`SlotPath`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathStep {
    /// An object key. Always a wire-format constant, never caller data, which
    /// is what makes `&'static str` safe here.
    Key(&'static str),
    /// A position in an array.
    Index(usize),
}

/// The address of one JSON string inside a request body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlotPath(Vec<PathStep>);

impl SlotPath {
    /// An empty path, addressing the body itself.
    pub fn root() -> Self {
        SlotPath(Vec::new())
    }

    /// Extend the path with an object key.
    #[must_use]
    pub fn key(mut self, key: &'static str) -> Self {
        self.0.push(PathStep::Key(key));
        self
    }

    /// Extend the path with an array index.
    #[must_use]
    pub fn index(mut self, index: usize) -> Self {
        self.0.push(PathStep::Index(index));
        self
    }

    /// The steps, in order from the root.
    pub fn steps(&self) -> &[PathStep] {
        &self.0
    }

    /// Follow the path, or `None` if any step is missing or the wrong kind.
    pub fn resolve<'a>(&self, root: &'a Value) -> Option<&'a Value> {
        let mut here = root;
        for step in &self.0 {
            here = match step {
                PathStep::Key(key) => here.as_object()?.get(*key)?,
                PathStep::Index(index) => here.as_array()?.get(*index)?,
            };
        }
        Some(here)
    }

    /// Follow the path for writing. Never creates missing steps: a body that no
    /// longer has the shape the walk saw is left alone rather than grown a key.
    pub fn resolve_mut<'a>(&self, root: &'a mut Value) -> Option<&'a mut Value> {
        let mut here = root;
        for step in &self.0 {
            here = match step {
                PathStep::Key(key) => here.as_object_mut()?.get_mut(*key)?,
                PathStep::Index(index) => here.as_array_mut()?.get_mut(*index)?,
            };
        }
        Some(here)
    }
}

impl fmt::Display for SlotPath {
    /// Renders as `messages[3].content[2].text`, which is what the proxy logs
    /// and what test failures read best.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, step) in self.0.iter().enumerate() {
            match step {
                PathStep::Key(key) => {
                    if position > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(key)?;
                }
                PathStep::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_path_renders_the_way_the_wire_format_reads() {
        let path = SlotPath::root()
            .key("messages")
            .index(3)
            .key("content")
            .index(2)
            .key("text");
        assert_eq!(path.to_string(), "messages[3].content[2].text");
    }

    #[test]
    fn a_path_resolves_to_the_value_it_addresses() {
        let body = json!({"messages": [{"content": [{"text": "hello"}]}]});
        let path = SlotPath::root()
            .key("messages")
            .index(0)
            .key("content")
            .index(0)
            .key("text");
        assert_eq!(path.resolve(&body).and_then(Value::as_str), Some("hello"));
    }

    #[test]
    fn resolving_a_missing_step_yields_none_instead_of_creating_it() {
        let mut body = json!({"messages": []});
        let path = SlotPath::root().key("messages").index(0).key("content");
        assert!(path.resolve(&body).is_none());
        assert!(path.resolve_mut(&mut body).is_none());
        assert_eq!(body, json!({"messages": []}), "nothing was grown");
    }
}
