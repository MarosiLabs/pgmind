//! Frontmatter handling (RFC-002 D4).
//!
//! A leading `---` … `---` YAML *mapping* becomes note-level properties.
//! Invalid YAML, a non-mapping, or YAML features beyond plain
//! mappings/sequences/scalars are treated as ordinary content — never an error.
//! We hand-strip the frontmatter before comrak sees the document (keeps the
//! D1 comrak option list literal) and account for the byte offset.

use serde_json::{Map, Value};
use yaml_rust2::{Yaml, YamlLoader};

pub(crate) struct Frontmatter {
    /// Byte length of the frontmatter section (0 if none), including the
    /// closing delimiter line and its newline.
    pub len: usize,
    /// Parsed properties; `{}` when absent or invalid-as-content.
    pub properties: Value,
    /// Tags from the reserved `tags` key (string or list of strings).
    pub tags: Vec<String>,
    /// Reserved `pgmind-pin` key (consumed by RFC-008).
    pub pin: bool,
}

impl Frontmatter {
    fn none() -> Self {
        Frontmatter {
            len: 0,
            properties: Value::Object(Map::new()),
            tags: vec![],
            pin: false,
        }
    }
}

/// Detect and parse frontmatter at the start of `source`.
pub(crate) fn extract(source: &str) -> Frontmatter {
    let Some(body) = source.strip_prefix("---") else {
        return Frontmatter::none();
    };
    // Opening delimiter must be exactly `---` followed by a newline.
    let Some(body) = body
        .strip_prefix('\n')
        .or_else(|| body.strip_prefix("\r\n"))
    else {
        return Frontmatter::none();
    };
    let opening_len = source.len() - body.len();

    // Find the closing delimiter: a line that is exactly `---` (or `...` is NOT
    // accepted — Obsidian requires `---`).
    let mut offset = 0usize;
    let yaml_text: &str;
    let close_line_len: usize;
    loop {
        let rest = &body[offset..];
        let line_end = rest
            .find('\n')
            .map(|i| offset + i + 1)
            .unwrap_or(body.len());
        let line = &body[offset..line_end];
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            yaml_text = &body[..offset];
            close_line_len = line.len();
            break;
        }
        if line_end == body.len() {
            // No closing delimiter: not frontmatter.
            return Frontmatter::none();
        }
        offset = line_end;
    }

    let total_len = opening_len + offset + close_line_len;

    // Parse the YAML; anything unusual → invalid-as-content (i.e. no frontmatter).
    let docs = match YamlLoader::load_from_str(yaml_text) {
        Ok(docs) => docs,
        Err(_) => return Frontmatter::none(),
    };
    // Multi-doc or empty → invalid per D4 (a single mapping is required).
    if docs.len() != 1 {
        return Frontmatter::none();
    }
    let Yaml::Hash(_) = &docs[0] else {
        return Frontmatter::none();
    };
    let Some(properties) = yaml_to_json(&docs[0]) else {
        return Frontmatter::none();
    };

    let mut tags = vec![];
    let mut pin = false;
    if let Value::Object(map) = &properties {
        match map.get("tags") {
            Some(Value::String(s)) => tags.push(s.clone()),
            Some(Value::Array(items)) => {
                for item in items {
                    if let Value::String(s) = item {
                        tags.push(s.clone());
                    }
                }
            }
            _ => {}
        }
        if let Some(Value::Bool(true)) = map.get("pgmind-pin") {
            pin = true;
        }
    }

    Frontmatter {
        len: total_len,
        properties,
        tags,
        pin,
    }
}

/// Plain mappings/sequences/scalars only (D4); anything else → None (invalid).
fn yaml_to_json(yaml: &Yaml) -> Option<Value> {
    Some(match yaml {
        Yaml::Real(s) => match s.parse::<f64>() {
            Ok(f) => serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::String(s.clone())),
            Err(_) => Value::String(s.clone()),
        },
        Yaml::Integer(i) => Value::Number((*i).into()),
        Yaml::String(s) => Value::String(s.clone()),
        Yaml::Boolean(b) => Value::Bool(*b),
        Yaml::Null => Value::Null,
        Yaml::Array(items) => {
            Value::Array(items.iter().map(yaml_to_json).collect::<Option<Vec<_>>>()?)
        }
        Yaml::Hash(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let Yaml::String(key) = k else { return None };
                out.insert(key.clone(), yaml_to_json(v)?);
            }
            Value::Object(out)
        }
        // Aliases, bad values, and anything exotic → invalid.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent() {
        assert_eq!(extract("# hi\n").len, 0);
    }

    #[test]
    fn valid_mapping() {
        let fm = extract("---\ntitle: Auth\ntags: [a, b]\npgmind-pin: true\n---\nbody\n");
        assert!(fm.len > 0);
        assert_eq!(fm.properties["title"], "Auth");
        assert_eq!(fm.tags, vec!["a", "b"]);
        assert!(fm.pin);
    }

    #[test]
    fn tags_scalar() {
        let fm = extract("---\ntags: solo\n---\n");
        assert_eq!(fm.tags, vec!["solo"]);
    }

    #[test]
    fn invalid_yaml_is_content() {
        assert_eq!(extract("---\n: : :\n---\n").len, 0);
    }

    #[test]
    fn non_mapping_is_content() {
        assert_eq!(extract("---\n- just\n- a list\n---\n").len, 0);
    }

    #[test]
    fn unclosed_is_content() {
        assert_eq!(extract("---\ntitle: x\n").len, 0);
    }

    #[test]
    fn thematic_break_lookalike() {
        // `---` followed by a blank line then `---` IS frontmatter with empty
        // YAML → empty YAML parses to zero docs → invalid → content.
        assert_eq!(extract("---\n\n---\n").len, 0);
    }
}
