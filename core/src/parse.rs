//! Document parsing (RFC-002 D1/D2/D5/D6): comrak walk → block taxonomy,
//! byte spans, heading paths, top-level tiling, extraction inputs.

use comrak::nodes::{AstNode, ListType, NodeValue, Sourcepos};
use comrak::{parse_document, Arena, Options};
use serde::Serialize;
use serde_json::{json, Value};
use unicode_normalization::UnicodeNormalization;

use crate::content::{normalized_content, Strip};
use crate::frontmatter;
use crate::span::{LineOffsets, Span};
use crate::vault;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Heading,
    Paragraph,
    ListItem,
    CodeBlock,
    Table,
    ThematicBreak,
    HtmlBlock,
}

impl BlockKind {
    /// Stable tag prefixed into the content hash (RFC-002 D7).
    pub fn tag(&self) -> &'static str {
        match self {
            BlockKind::Heading => "heading",
            BlockKind::Paragraph => "paragraph",
            BlockKind::ListItem => "list_item",
            BlockKind::CodeBlock => "code_block",
            BlockKind::Table => "table",
            BlockKind::ThematicBreak => "thematic_break",
            BlockKind::HtmlBlock => "html_block",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Block {
    pub ord: u32,
    pub kind: BlockKind,
    /// Index of the enclosing addressable block (list_item nesting), if any.
    pub parent: Option<u32>,
    /// Heading text of enclosing headings, outermost first (RFC-002 D2).
    pub heading_path: Vec<String>,
    /// Byte span in the ORIGINAL source. Top-level blocks tile the document
    /// (with preamble); nested blocks are sub-ranges of their ancestors (D6).
    pub span: Span,
    /// BLAKE3 over kind_tag ‖ 0x00 ‖ normalized_content (D7).
    #[serde(serialize_with = "hex_hash")]
    pub content_hash: [u8; 32],
    /// The normalized logical content that was hashed (exposed for goldens/debug).
    pub normalized_content: String,
    /// Opt-in `^id` marker (D3), stripped from content.
    pub block_ref_id: Option<String>,
    pub attrs: Value,
    /// Canonical decoration for a NEW first line at this block's container
    /// nesting (for list items: excluding the item's own marker strip).
    /// Splice aid for RFC-003 D6 — not part of the serialized output contract.
    #[serde(skip)]
    pub line_prefix: String,
    /// Canonical decoration for continuation lines of this block (full
    /// container stack: quote prefixes + item continuation indents).
    #[serde(skip)]
    pub cont_prefix: String,
    /// Bytes of decoration on the block's first source line, including the
    /// block's own item marker when it has one. Splice aid (RFC-003 D6).
    #[serde(skip)]
    pub first_line_strip_full: usize,
    /// Same, but excluding the block's own item strip (for list items: the
    /// outer quote/item decoration only, marker not counted).
    #[serde(skip)]
    pub first_line_strip_outer: usize,
}

fn hex_hash<S: serde::Serializer>(h: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
    let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
    s.serialize_str(&hex)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Wikilink,
    Transclusion,
    Mdlink,
    Blockref,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkRef {
    pub kind: LinkKind,
    /// Target as written, whitespace-trimmed and NFC-normalized (D3/D8).
    pub target: String,
    pub anchor: Option<String>,
    pub alias: Option<String>,
    /// ord of the block the reference appears in.
    pub block: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagRef {
    /// Tag as written (index matches case-insensitively downstream).
    pub tag: String,
    /// ord of the block, or None for frontmatter tags (note-level).
    pub block: Option<u32>,
}

/// A scannable run of plain text (D3).
#[derive(Debug, Clone)]
pub(crate) enum TextUnit {
    /// Byte range into the original source (normal inline text).
    Range(Span),
    /// Reconstructed literal (table cells: GFM resolves `\|` before inline
    /// parsing and its sourcepos does not map back to raw source bytes).
    Literal(String),
}

#[derive(Debug, Serialize)]
pub struct Document {
    /// Frontmatter + leading trivia (D6). May be empty.
    pub preamble: Span,
    /// `{}` when absent or invalid-as-content (D4).
    pub properties: Value,
    pub pinned: bool,
    pub blocks: Vec<Block>,
    /// Spans of top-level document children (containers as one span each),
    /// tiling the source together with the preamble (D6).
    pub top_level: Vec<Span>,
    pub links: Vec<LinkRef>,
    pub tags: Vec<TagRef>,
}

impl Document {
    /// D6 gate property: preamble + top-level spans reproduce the source exactly.
    pub fn tiles_exactly(&self, source: &str) -> bool {
        let mut pos = self.preamble.end;
        if self.preamble.start != 0 {
            return false;
        }
        for s in &self.top_level {
            if s.start != pos {
                return false;
            }
            pos = s.end;
        }
        pos == source.len()
    }
}

pub(crate) fn comrak_options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.tasklist = true;
    options.extension.strikethrough = true;
    // autolink, footnotes, superscript, description_lists: off (D1).
    // parse.smart: off (default).
    options
}

struct Ctx<'a> {
    source: &'a str, // original, full
    base: usize,     // frontmatter offset: md coords + base = original coords
    lo: LineOffsets, // over the markdown body (source[base..])
    blocks: Vec<Block>,
    /// Per block: text units of its own plain-text inline nodes, in document
    /// order — the only places vault syntax is recognized (D3).
    text_ranges: Vec<Vec<TextUnit>>,
    /// Per block: own-content line ranges (md coords, 1-based, inclusive).
    own_lines: Vec<Vec<(usize, usize)>>,
    /// Per block: container stripping stack (outermost first).
    strips: Vec<Vec<Strip>>,
    /// Per block: first source line (md coords, 1-based) — splice-aid input.
    first_lines: Vec<usize>,
    heading_stack: Vec<(u8, String)>,
    /// Attrs of enclosing List containers (ordered/start/tight) for items.
    list_stack: Vec<Value>,
    links: Vec<LinkRef>,
    tags: Vec<TagRef>,
}

impl<'a> Ctx<'a> {
    fn md(&self) -> &'a str {
        &self.source[self.base..]
    }
    fn to_original(&self, span: Span) -> Span {
        Span {
            start: span.start + self.base,
            end: span.end + self.base,
        }
    }
}

/// Parse a document per RFC-002. `source` must be valid UTF-8 (guaranteed by &str).
pub fn parse(source: &str) -> Document {
    let fm = frontmatter::extract(source);
    let base = fm.len;
    let md = &source[base..];

    let arena = Arena::new();
    let root = parse_document(&arena, md, &comrak_options());

    let mut ctx = Ctx {
        source,
        base,
        lo: LineOffsets::new(md),
        blocks: vec![],
        text_ranges: vec![],
        own_lines: vec![],
        strips: vec![],
        first_lines: vec![],
        heading_stack: vec![],
        list_stack: vec![],
        links: vec![],
        tags: vec![],
    };

    // Walk top-level children, collecting their raw line spans first.
    let mut top_nodes: Vec<&AstNode> = vec![];
    for child in root.children() {
        top_nodes.push(child);
    }

    for node in &top_nodes {
        walk_block(node, &mut ctx, &mut vec![], None);
    }

    // Top-level tiling (D6): span i runs from its first line start to the next
    // top-level node's first line start (or EOF), attaching trailing trivia.
    let mut top_level: Vec<Span> = vec![];
    let starts: Vec<usize> = top_nodes
        .iter()
        .map(|n| ctx.lo.line_start(n.data.borrow().sourcepos.start.line))
        .collect();
    for (i, &s) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(md.len());
        top_level.push(Span {
            start: s + base,
            end: end + base,
        });
    }
    let preamble_end = top_level.first().map(|s| s.start).unwrap_or(source.len());
    let preamble = Span {
        start: 0,
        end: preamble_end,
    };

    // Top-level (non-container) blocks adopt their tile so trailing trivia
    // attaches; nested blocks keep their line spans (sub-ranges, D5/D6).
    // Both top_nodes and ctx.blocks are in document order — single cursor.
    let mut block_cursor = 0usize;
    for (node, tile) in top_nodes.iter().zip(top_level.iter()) {
        if is_container(node) {
            continue;
        }
        let sp = node.data.borrow().sourcepos;
        let node_start = ctx.lo.line_start(sp.start.line) + base;
        while block_cursor < ctx.blocks.len() && ctx.blocks[block_cursor].span.start < node_start {
            block_cursor += 1;
        }
        if let Some(b) = ctx.blocks.get_mut(block_cursor) {
            if b.parent.is_none() && b.span.start == node_start {
                b.span = *tile;
                block_cursor += 1;
            }
        }
    }

    // Vault syntax pass (D3): scan text units per block.
    vault::scan(&ctx.text_ranges, ctx.source, &mut ctx.links, &mut ctx.tags);

    // Block-ID markers (D3): recognized on the container-stripped last content
    // line, for heading/paragraph/list_item; innermost eligible block wins.
    let mut has_child = vec![false; ctx.blocks.len()];
    for b in &ctx.blocks {
        if let Some(p) = b.parent {
            has_child[p as usize] = true;
        }
    }
    #[allow(clippy::needless_range_loop)] // parallel arrays share the index
    for i in 0..ctx.blocks.len() {
        let kind = ctx.blocks[i].kind;
        if !matches!(
            kind,
            BlockKind::Heading | BlockKind::Paragraph | BlockKind::ListItem
        ) || (kind == BlockKind::ListItem && has_child[i])
        {
            continue;
        }
        // Last content line: setext headings exclude their underline.
        let Some(&(first, last)) = ctx.own_lines[i].last() else {
            continue;
        };
        let setext = ctx.blocks[i].attrs.get("setext").and_then(Value::as_bool) == Some(true);
        let line_no = if setext && last > first {
            last - 1
        } else {
            last
        };
        let line_span = ctx.lo.line_span(line_no, line_no);
        if line_span.start >= ctx.md().len() {
            continue;
        }
        let raw = ctx.md()[line_span.start..line_span.end.min(ctx.md().len())]
            .trim_end_matches(['\n', '\r']);
        let logical = crate::content::strip_containers(raw, line_no, &ctx.strips[i])
            .trim_end_matches([' ', '\t']);
        let Some(id) = vault::trailing_ref_marker(logical) else {
            continue;
        };
        // Guard: the marker must sit inside a plain-text inline range (not code).
        let marker = format!(" ^{id}");
        let Some(rel) = raw.trim_end_matches([' ', '\t']).rfind(&marker) else {
            continue;
        };
        let marker_pos = line_span.start + base + rel + 1; // at '^'
        let in_text = ctx.text_ranges[i].iter().any(|u| match u {
            TextUnit::Range(r) => marker_pos >= r.start && marker_pos < r.end,
            TextUnit::Literal(_) => false,
        });
        if in_text {
            ctx.blocks[i].block_ref_id = Some(id.to_string());
            if let Value::Object(map) = &mut ctx.blocks[i].attrs {
                map.insert("block_ref_id".into(), Value::String(id.to_string()));
            }
        }
    }

    // D7 step 4: a marker line shared with enclosing items must be stripped
    // from THEIR hashes too — propagate up the parent chain.
    let mut hash_markers: Vec<Option<String>> =
        ctx.blocks.iter().map(|b| b.block_ref_id.clone()).collect();
    #[allow(clippy::needless_range_loop)] // walks parent chain by index
    for i in 0..ctx.blocks.len() {
        if let Some(id) = ctx.blocks[i].block_ref_id.clone() {
            let mut p = ctx.blocks[i].parent;
            while let Some(pi) = p {
                let pi = pi as usize;
                if hash_markers[pi].is_none() {
                    hash_markers[pi] = Some(id.clone());
                }
                p = ctx.blocks[pi].parent;
            }
        }
    }

    // Compute normalized content + hashes (D7) — after ^id markers are known.
    #[allow(clippy::needless_range_loop)] // parallel arrays share the index
    for i in 0..ctx.blocks.len() {
        let normalized = normalized_content(
            ctx.md(),
            &ctx.lo,
            &ctx.own_lines[i],
            &ctx.strips[i],
            hash_markers[i].as_deref(),
        );
        ctx.blocks[i].content_hash = crate::content_hash(ctx.blocks[i].kind, &normalized);
        ctx.blocks[i].normalized_content = normalized;

        // Splice aids (RFC-003 D6): decoration byte widths on the first line.
        let fl = ctx.first_lines[i];
        let lsp = ctx.lo.line_span(fl, fl);
        if lsp.start < ctx.md().len() {
            let raw = &ctx.md()[lsp.start..lsp.end.min(ctx.md().len())];
            let line = raw.trim_end_matches(['\n', '\r']);
            let full_rem = crate::content::strip_containers(line, fl, &ctx.strips[i]);
            let outer_strips: &[Strip] = if ctx.blocks[i].kind == BlockKind::ListItem {
                &ctx.strips[i][..ctx.strips[i].len().saturating_sub(1)]
            } else {
                &ctx.strips[i]
            };
            let outer_rem = crate::content::strip_containers(line, fl, outer_strips);
            ctx.blocks[i].first_line_strip_full = line.len() - full_rem.len();
            ctx.blocks[i].first_line_strip_outer = line.len() - outer_rem.len();
        }
    }

    let mut tags = ctx.tags;
    for t in fm.tags {
        tags.push(TagRef {
            tag: t,
            block: None,
        });
    }

    Document {
        preamble,
        properties: fm.properties,
        pinned: fm.pin,
        blocks: ctx.blocks,
        top_level,
        links: ctx.links,
        tags,
    }
}

fn is_container(node: &AstNode) -> bool {
    matches!(
        node.data.borrow().value,
        NodeValue::List(_) | NodeValue::BlockQuote
    )
}

/// Recursive block walk. `strips` = container stripping stack for children.
fn walk_block<'a>(
    node: &'a AstNode<'a>,
    ctx: &mut Ctx,
    strips: &mut Vec<Strip>,
    parent_block: Option<u32>,
) {
    let (value_kind, sp) = {
        let data = node.data.borrow();
        (discriminant_of(&data.value), data.sourcepos)
    };

    match value_kind {
        Disc::BlockQuote => {
            strips.push(Strip::Quote);
            for child in node.children() {
                walk_block(child, ctx, strips, parent_block);
            }
            strips.pop();
        }
        Disc::List => {
            let list_attrs = {
                let data = node.data.borrow();
                if let NodeValue::List(l) = &data.value {
                    json!({
                        "ordered": l.list_type == ListType::Ordered,
                        "start": l.start,
                        "tight": l.tight,
                    })
                } else {
                    json!({})
                }
            };
            ctx.list_stack.push(list_attrs);
            for child in node.children() {
                walk_block(child, ctx, strips, parent_block);
            }
            ctx.list_stack.pop();
        }
        Disc::Item => {
            let (first_width, cont_width, item_attrs) = item_geometry(node, ctx, sp, strips);
            // The item's OWN strip is part of its stack too: its marker (and a
            // task item's checkbox) is decoration, not content (D7 step 2).
            strips.push(Strip::Item {
                first_line: sp.start.line,
                first_width,
                cont_width,
            });
            let ord = push_block(
                ctx,
                BlockKind::ListItem,
                sp,
                parent_block,
                strips,
                item_attrs,
            );
            // Own content: direct non-container children (D7 step 3 as
            // amended: nested lists/quotes and their descendants excluded).
            let mut own: Vec<(usize, usize)> = vec![];
            for child in node.children() {
                if !is_container(child) {
                    let csp = child.data.borrow().sourcepos;
                    own.push((csp.start.line, csp.end.line));
                }
            }
            if own.is_empty() {
                own.push((sp.start.line, sp.start.line));
            }
            ctx.own_lines[ord as usize] = own;
            for child in node.children() {
                walk_block(child, ctx, strips, Some(ord));
            }
            strips.pop();
        }
        Disc::Heading => {
            let (level, setext) = heading_info(node);
            while ctx.heading_stack.last().is_some_and(|(l, _)| *l >= level) {
                ctx.heading_stack.pop();
            }
            let ord = push_block(
                ctx,
                BlockKind::Heading,
                sp,
                parent_block,
                strips,
                json!({ "level": level, "setext": setext }),
            );
            ctx.own_lines[ord as usize] = vec![(sp.start.line, sp.end.line)];
            collect_text_ranges(node, ctx, ord);
            let text = heading_text(node, ctx);
            // The heading's own text (D2's single definition) rides in attrs so
            // consumers (read_section, Phase 5 append_to_section) never re-derive it.
            if let Value::Object(map) = &mut ctx.blocks[ord as usize].attrs {
                map.insert("text".into(), Value::String(text.clone()));
            }
            ctx.heading_stack.push((level, text));
        }
        Disc::Paragraph => {
            let ord = push_block(
                ctx,
                BlockKind::Paragraph,
                sp,
                parent_block,
                strips,
                json!({}),
            );
            ctx.own_lines[ord as usize] = vec![(sp.start.line, sp.end.line)];
            collect_text_ranges(node, ctx, ord);
        }
        Disc::CodeBlock => {
            let attrs = {
                let data = node.data.borrow();
                if let NodeValue::CodeBlock(cb) = &data.value {
                    json!({ "fenced": cb.fenced, "info": cb.info })
                } else {
                    json!({})
                }
            };
            let ord = push_block(ctx, BlockKind::CodeBlock, sp, parent_block, strips, attrs);
            ctx.own_lines[ord as usize] = vec![(sp.start.line, sp.end.line)];
        }
        Disc::HtmlBlock => {
            let ord = push_block(
                ctx,
                BlockKind::HtmlBlock,
                sp,
                parent_block,
                strips,
                json!({}),
            );
            ctx.own_lines[ord as usize] = vec![(sp.start.line, sp.end.line)];
        }
        Disc::ThematicBreak => {
            let ord = push_block(
                ctx,
                BlockKind::ThematicBreak,
                sp,
                parent_block,
                strips,
                json!({}),
            );
            ctx.own_lines[ord as usize] = vec![(sp.start.line, sp.end.line)];
        }
        Disc::Table => {
            let ord = push_block(ctx, BlockKind::Table, sp, parent_block, strips, json!({}));
            ctx.own_lines[ord as usize] = vec![(sp.start.line, sp.end.line)];
            // Scan cell text for tags/links (block = the table) — as literals,
            // since cell sourcepos points into GFM-reconstructed content.
            collect_cell_literals(node, ctx, ord);
        }
        Disc::Other => {
            // Unhandled block-level nodes: recurse so nothing is silently lost.
            for child in node.children() {
                walk_block(child, ctx, strips, parent_block);
            }
        }
    }
}

enum Disc {
    BlockQuote,
    List,
    Item,
    Heading,
    Paragraph,
    CodeBlock,
    HtmlBlock,
    ThematicBreak,
    Table,
    Other,
}

fn discriminant_of(v: &NodeValue) -> Disc {
    match v {
        NodeValue::BlockQuote => Disc::BlockQuote,
        NodeValue::List(_) => Disc::List,
        NodeValue::Item(_) | NodeValue::TaskItem(_) => Disc::Item,
        NodeValue::Heading(_) => Disc::Heading,
        NodeValue::Paragraph => Disc::Paragraph,
        NodeValue::CodeBlock(_) => Disc::CodeBlock,
        NodeValue::HtmlBlock(_) => Disc::HtmlBlock,
        NodeValue::ThematicBreak => Disc::ThematicBreak,
        NodeValue::Table(_) => Disc::Table,
        _ => Disc::Other,
    }
}

fn heading_info(node: &AstNode) -> (u8, bool) {
    if let NodeValue::Heading(h) = &node.data.borrow().value {
        (h.level, h.setext)
    } else {
        (0, false)
    }
}

/// Item stripping geometry → (first_width, cont_width, attrs).
/// Widths are relative to the line AFTER the enclosing containers' prefixes
/// are stripped. For plain items comrak supplies marker_offset+padding
/// directly; for task items comrak discards the list geometry (NodeTaskItem),
/// so it is re-derived from source columns: the checkbox symbol sits at
/// `symbol_sourcepos`, content starts 3 columns later ("[x] "), and the
/// enclosing-prefix width is measured by stripping the raw first line.
fn item_geometry(
    node: &AstNode,
    ctx: &Ctx,
    sp: Sourcepos,
    outer: &[Strip],
) -> (usize, usize, Value) {
    let data = node.data.borrow();
    match &data.value {
        NodeValue::Item(l) => {
            let width = l.marker_offset + l.padding;
            let mut attrs = json!({
                "ordered": l.list_type == ListType::Ordered,
                "start": l.start,
                "tight": l.tight,
            });
            if let Some(list) = ctx.list_stack.last() {
                merge_json(&mut attrs, list);
            }
            (width, width, attrs)
        }
        NodeValue::TaskItem(task) => {
            let sym_col = task.symbol_sourcepos.start.column;
            let item_col = sp.start.column;
            // Enclosing-prefix bytes consumed on the item's first raw line.
            let line_span = ctx.lo.line_span(sp.start.line, sp.start.line);
            let raw = ctx.md()[line_span.start..line_span.end.min(ctx.md().len())]
                .trim_end_matches(['\n', '\r']);
            let consumed =
                raw.len() - crate::content::strip_containers(raw, sp.start.line, outer).len();
            let rel_offset = (item_col.saturating_sub(1)).saturating_sub(consumed);
            // Marker ("- ", "1. ", …) ends where the checkbox '[' begins.
            let marker_width = sym_col.saturating_sub(1).saturating_sub(item_col);
            // Content starts after "[x] " (checkbox + closing bracket + space).
            let first_width = rel_offset + sym_col.saturating_sub(item_col) + 3;
            let cont_width = rel_offset + marker_width;
            let mut attrs = json!({ "task": true, "checked": task.symbol.is_some() });
            if let Some(list) = ctx.list_stack.last() {
                merge_json(&mut attrs, list);
            }
            (first_width, cont_width, attrs)
        }
        _ => (0, 0, json!({})),
    }
}

/// Copy fields of `from` into `into` without overwriting existing keys.
fn merge_json(into: &mut Value, from: &Value) {
    if let (Value::Object(into), Value::Object(from)) = (into, from) {
        for (k, v) in from {
            into.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

fn push_block(
    ctx: &mut Ctx,
    kind: BlockKind,
    sp: Sourcepos,
    parent: Option<u32>,
    strips: &[Strip],
    attrs: Value,
) -> u32 {
    let ord = ctx.blocks.len() as u32;
    let span = ctx.to_original(ctx.lo.line_span(sp.start.line, sp.end.line));
    let render = |ss: &[Strip]| -> String {
        ss.iter()
            .map(|s| match s {
                Strip::Quote => "> ".to_string(),
                Strip::Item { cont_width, .. } => " ".repeat(*cont_width),
            })
            .collect()
    };
    let cont_prefix = render(strips);
    let line_prefix = if kind == BlockKind::ListItem {
        render(&strips[..strips.len().saturating_sub(1)])
    } else {
        cont_prefix.clone()
    };
    ctx.blocks.push(Block {
        ord,
        kind,
        parent,
        heading_path: ctx.heading_stack.iter().map(|(_, t)| t.clone()).collect(),
        span,
        content_hash: [0; 32],
        normalized_content: String::new(),
        block_ref_id: None,
        attrs,
        line_prefix,
        cont_prefix,
        first_line_strip_full: 0,
        first_line_strip_outer: 0,
    });
    ctx.text_ranges.push(vec![]);
    ctx.own_lines.push(vec![]);
    ctx.strips.push(strips.to_vec());
    ctx.first_lines.push(sp.start.line);
    ord
}

/// Collect the byte ranges (original coords) of plain Text inline nodes under
/// `node`, skipping Link/Image subtrees (CommonMark wins — D3) and all code.
fn collect_text_ranges<'a>(node: &'a AstNode<'a>, ctx: &mut Ctx, block: u32) {
    for child in node.children() {
        let data = child.data.borrow();
        match &data.value {
            NodeValue::Text(_) => {
                let sp = data.sourcepos;
                let md = ctx.md();
                let mut start =
                    ctx.lo.line_start(sp.start.line) + sp.start.column.saturating_sub(1);
                let mut end = (ctx.lo.line_start(sp.end.line) + sp.end.column).min(md.len());
                // comrak columns are byte-counted but its internal tab expansion
                // can skew them — snap to char boundaries so slicing is safe.
                while start < md.len() && !md.is_char_boundary(start) {
                    start += 1;
                }
                while end > 0 && !md.is_char_boundary(end) {
                    end -= 1;
                }
                if start < end {
                    let span = ctx.to_original(Span { start, end });
                    ctx.text_ranges[block as usize].push(TextUnit::Range(span));
                }
            }
            NodeValue::Link(link) => {
                // mdlink extraction (D3); do not descend (its text is occluded).
                vault::record_mdlink(&link.url, block, &mut ctx.links);
            }
            NodeValue::Image(_) | NodeValue::Code(_) | NodeValue::HtmlInline(_) => {}
            _ => {
                drop(data);
                collect_text_ranges(child, ctx, block);
            }
        }
    }
}

/// Table-cell text (D3 cell semantics): collect each cell Text node's literal
/// content; mdlinks inside cells still extract via the Link arm.
fn collect_cell_literals<'a>(node: &'a AstNode<'a>, ctx: &mut Ctx, block: u32) {
    for child in node.children() {
        let data = child.data.borrow();
        match &data.value {
            NodeValue::Text(t) => {
                ctx.text_ranges[block as usize].push(TextUnit::Literal(t.to_string()));
            }
            NodeValue::Link(link) => {
                vault::record_mdlink(&link.url, block, &mut ctx.links);
            }
            NodeValue::Image(_) | NodeValue::Code(_) | NodeValue::HtmlInline(_) => {}
            _ => {
                drop(data);
                collect_cell_literals(child, ctx, block);
            }
        }
    }
}

/// Heading text (D2): plain inline text — formatting dropped, wiki-links
/// contribute alias-else-target, code spans literal, `^id` stripped — NFC, trimmed.
fn heading_text<'a>(node: &'a AstNode<'a>, _ctx: &Ctx) -> String {
    let mut out = String::new();
    let mut last_from_text = false;
    collect_plain_text(node, &mut out, &mut last_from_text);
    let out = vault::replace_wikilinks_with_display(&out);
    // Strip a trailing ^id only when it is plain text (a code span ending in
    // caret-word is content, not a marker — mirrors marker recognition).
    let out = if last_from_text {
        vault::strip_ref_marker_text(&out)
    } else {
        out
    };
    let text: String = out.nfc().collect();
    text.trim().to_string()
}

fn collect_plain_text<'a>(node: &'a AstNode<'a>, out: &mut String, last_from_text: &mut bool) {
    for child in node.children() {
        let data = child.data.borrow();
        match &data.value {
            NodeValue::Text(t) => {
                out.push_str(t);
                *last_from_text = true;
            }
            NodeValue::Code(c) => {
                out.push_str(&c.literal);
                *last_from_text = false;
            }
            NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
            NodeValue::HtmlInline(_) => {}
            _ => {
                drop(data);
                collect_plain_text(child, out, last_from_text);
            }
        }
    }
}
