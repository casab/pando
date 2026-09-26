//! The YAML subset a compose file is read with, and the services, ports
//! and mounts it yields.

use super::ComposeFile;
use super::Mount;
use super::Port;
use super::Service;
use super::TopVolume;
use anyhow::{Result, bail};

// ---- parsing --------------------------------------------------------------

pub fn parse(text: &str) -> Result<ComposeFile> {
    let node = parse_document(text)?;
    let Node::Map(top) = node else {
        bail!("a compose file is a mapping at its top level");
    };
    let mut file = ComposeFile::default();
    // Only under the two keys this reader takes anything from: an alias in
    // an `x-` block that nothing here reads changes nothing pando decides.
    file.unresolved.aliases = top.iter().any(|(key, value)| {
        matches!(key.as_str(), "services" | "volumes") && refers_elsewhere(value)
    });
    for (key, value) in &top {
        match key.as_str() {
            "services" => {
                let Node::Map(services) = value else { continue };
                for (name, body) in services {
                    if has_key(body, "extends") {
                        file.unresolved.extends.push(name.clone());
                    }
                    file.services.insert(name.clone(), service(body));
                }
            }
            "volumes" => {
                let Node::Map(volumes) = value else { continue };
                for (name, body) in volumes {
                    file.volumes.insert(name.clone(), top_volume(body));
                }
            }
            // Not followed, but not ignored either: it brings in services
            // this file never names, so a refusal about "what it declares"
            // would be untrue of the file compose reads.
            "include" => file.unresolved.include = true,
            _ => {}
        }
    }
    Ok(file)
}

fn has_key(node: &Node, want: &str) -> bool {
    matches!(node, Node::Map(fields) if fields.iter().any(|(key, _)| key == want))
}

/// Whether anything under `node` is an alias or a merge key: text written
/// somewhere else in the file, which this reader does not copy in.
fn refers_elsewhere(node: &Node) -> bool {
    match node {
        Node::Alias(_) => true,
        Node::Scalar(_) => false,
        Node::Seq(items) => items.iter().any(refers_elsewhere),
        Node::Map(fields) => fields
            .iter()
            .any(|(key, value)| key == "<<" || refers_elsewhere(value)),
    }
}

fn service(node: &Node) -> Service {
    let mut out = Service::default();
    let Node::Map(fields) = node else {
        return out;
    };
    for (key, value) in fields {
        match key.as_str() {
            "image" => out.image = value.scalar().map(str::to_string),
            // `build: ./api`, or `build:` with a `context:` under it. A
            // mapping that names only a `dockerfile:` still has a context,
            // and compose's default for it is `.`.
            "build" => {
                out.build = Some(match value {
                    Node::Map(fields) => fields
                        .iter()
                        .find(|(key, _)| key == "context")
                        .and_then(|(_, value)| value.scalar())
                        .unwrap_or(".")
                        .to_string(),
                    other => other.scalar().unwrap_or(".").to_string(),
                })
            }
            "container_name" => out.container_name = value.scalar().map(str::to_string),
            // Only that it is there. What it runs is docker's business;
            // pando asks `docker compose ps` whether it passed.
            "healthcheck" => out.healthcheck = declares_healthcheck(value),
            "ports" => out.ports = value.items().iter().filter_map(port).collect(),
            "volumes" => out.volumes = value.items().iter().filter_map(mount).collect(),
            // Both forms: a list of names, or a map of name to condition.
            "depends_on" => {
                out.depends_on = match value {
                    Node::Map(entries) => entries.iter().map(|(name, _)| name.clone()).collect(),
                    other => other
                        .items()
                        .iter()
                        .filter_map(|item| item.scalar().map(str::to_string))
                        .collect(),
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether a `healthcheck:` block leaves docker a check to run.
/// `disable: true` and a `test:` list that starts with `NONE` are
/// compose's two ways of turning one off, and docker then reports no
/// health at all, so readiness waiting for `healthy` would never end.
/// An alias stands for a block this reader never sees, and counts.
fn declares_healthcheck(node: &Node) -> bool {
    let fields = match node {
        Node::Map(fields) => fields,
        Node::Alias(_) => return true,
        Node::Scalar(_) | Node::Seq(_) => return false,
    };
    let get = |want: &str| {
        fields
            .iter()
            .find(|(key, _)| key == want)
            .map(|(_, value)| value)
    };
    if get("disable").and_then(Node::scalar) == Some("true") {
        return false;
    }
    !matches!(
        get("test"),
        Some(Node::Seq(test)) if test.first().and_then(Node::scalar) == Some("NONE")
    )
}

fn top_volume(node: &Node) -> TopVolume {
    let mut out = TopVolume::default();
    let Node::Map(fields) = node else {
        return out;
    };
    for (key, value) in fields {
        match key.as_str() {
            "name" => out.name = value.scalar().map(str::to_string),
            // `external: { name: x }` is the older spelling and is just as
            // shared as `external: true`.
            "external" => out.external = matches!(value.scalar(), Some("true") | None),
            _ => {}
        }
    }
    out
}

fn port(node: &Node) -> Option<Port> {
    match node {
        Node::Scalar(text) => short_port(text),
        Node::Map(fields) => {
            let get = |want: &str| {
                fields
                    .iter()
                    .find(|(key, _)| key == want)
                    .and_then(|(_, value)| value.scalar())
            };
            Some(Port {
                container: first_of_range(get("target")?)?,
                published: get("published").and_then(first_of_range),
                host: get("host_ip").map(str::to_string),
            })
        }
        Node::Seq(_) | Node::Alias(_) => None,
    }
}

/// `"5432"`, `"5432:5432"`, `"127.0.0.1:5432:5432"`, `"[::1]:80:80"`,
/// `"9090-9091:8080-8081"`, `"6060:6060/udp"`.
fn short_port(text: &str) -> Option<Port> {
    let text = text.split('/').next().unwrap_or(text).trim();
    // An IPv6 host is bracketed, and its colons are not separators.
    let (host, rest) = match text.strip_prefix('[') {
        Some(after) => {
            let (inside, rest) = after.split_once(']')?;
            (
                Some(format!("[{inside}]")),
                rest.strip_prefix(':').unwrap_or(rest),
            )
        }
        None => (None, text),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    match (host, parts.as_slice()) {
        (None, [container]) => Some(Port {
            container: first_of_range(container)?,
            published: None,
            host: None,
        }),
        (host, [published, container]) => Some(Port {
            container: first_of_range(container)?,
            published: first_of_range(published),
            host,
        }),
        (None, [host, published, container]) => Some(Port {
            container: first_of_range(container)?,
            published: first_of_range(published),
            host: Some((*host).to_string()),
        }),
        _ => None,
    }
}

/// The first port of `"5000-5010"`, or of a plain `"5000"`. A range
/// published per worktree would need a range allocated, which roles cannot
/// express; the first is the one the app is told about.
pub(super) fn first_of_range(text: &str) -> Option<u16> {
    let text = text.trim().trim_matches('"').trim_matches('\'');
    let first = text.split('-').next().unwrap_or(text);
    first.trim().parse().ok()
}

fn mount(node: &Node) -> Option<Mount> {
    match node {
        Node::Scalar(text) => Some(short_mount(text)),
        Node::Map(fields) => {
            let get = |want: &str| {
                fields
                    .iter()
                    .find(|(key, _)| key == want)
                    .and_then(|(_, value)| value.scalar())
            };
            let source = get("source")?;
            match get("type") {
                Some("bind") => Some(Mount::Bind(source.to_string())),
                Some("volume") | None => Some(Mount::Named(source.to_string())),
                // tmpfs, npipe, cluster: nothing on the host to isolate.
                Some(_) => None,
            }
        }
        Node::Seq(_) | Node::Alias(_) => None,
    }
}

fn short_mount(text: &str) -> Mount {
    let text = text.trim();
    // `/data`, with no source at all: docker invents a per-container
    // volume, so there is nothing shared to isolate.
    let Some((source, _)) = text.split_once(':') else {
        return Mount::Anonymous;
    };
    if source.starts_with(['.', '/', '~', '$']) || source.contains('/') {
        return Mount::Bind(source.to_string());
    }
    Mount::Named(source.to_string())
}

/// The subset of YAML a compose file is written in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Scalar(String),
    Map(Vec<(String, Node)>),
    Seq(Vec<Node>),
    /// `*name`: whatever the anchor `&name` marks elsewhere in the file.
    /// Kept as a node of its own rather than read as the text `*name`, so
    /// the file can say it was not read whole.
    Alias(String),
}

impl Node {
    fn scalar(&self) -> Option<&str> {
        match self {
            Node::Scalar(text) if text.is_empty() => None,
            Node::Scalar(text) => Some(text),
            _ => None,
        }
    }

    /// The entries of a sequence; a lone scalar counts as one entry, which
    /// is how `depends_on: db` reads.
    fn items(&self) -> Vec<Node> {
        match self {
            Node::Seq(items) => items.clone(),
            Node::Scalar(text) if text.is_empty() => Vec::new(),
            other => vec![other.clone()],
        }
    }
}

/// One significant line: how far it is indented, and what is on it.
struct Line {
    indent: usize,
    text: String,
}

fn parse_document(text: &str) -> Result<Node> {
    let lines = significant_lines(text);
    if lines.is_empty() {
        return Ok(Node::Map(Vec::new()));
    }
    let mut at = 0usize;
    let node = parse_block(&lines, &mut at, lines[0].indent)?;
    Ok(node)
}

/// Comments and blank lines removed, tabs refused.
///
/// YAML forbids a tab as indentation, and a compose file indented with one
/// is a file docker itself will not read; saying so beats parsing it into
/// a shape that silently loses a service.
fn significant_lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let without_comment = strip_comment(raw);
        if without_comment.trim().is_empty() {
            continue;
        }
        let indent = without_comment.len() - without_comment.trim_start().len();
        out.push(Line {
            indent,
            text: without_comment.trim_end().to_string(),
        });
    }
    out
}

/// A `#` starts a comment only at the start of a token, and never inside a
/// quoted scalar: `image: "redis#7"` is an image name.
fn strip_comment(line: &str) -> String {
    let mut quote: Option<char> = None;
    let mut previous = ' ';
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' && (i == 0 || previous.is_whitespace()) => {
                return line[..i].to_string();
            }
            None => {}
        }
        previous = c;
    }
    line.to_string()
}

/// A block at `indent`: either a sequence of `-` entries or a mapping.
fn parse_block(lines: &[Line], at: &mut usize, indent: usize) -> Result<Node> {
    if lines
        .get(*at)
        .is_some_and(|line| line.text.trim_start().starts_with("- "))
        || lines.get(*at).is_some_and(|line| line.text.trim() == "-")
    {
        return parse_seq(lines, at, indent);
    }
    parse_map(lines, at, indent)
}

fn parse_seq(lines: &[Line], at: &mut usize, indent: usize) -> Result<Node> {
    let mut items = Vec::new();
    while let Some(line) = lines.get(*at) {
        if line.indent < indent {
            break;
        }
        let trimmed = line.text.trim_start();
        if line.indent > indent || !trimmed.starts_with('-') {
            break;
        }
        let rest = without_anchor(trimmed[1..].trim_start());
        // The column the entry's own content starts at, so `- target: 80`
        // followed by `  published: 8080` reads as one mapping.
        let inner = line.indent + (trimmed.len() - rest.len());
        let rest = rest.to_string();
        *at += 1;
        if rest.is_empty() {
            let child_indent = lines.get(*at).map(|l| l.indent).unwrap_or(indent);
            if child_indent > indent {
                items.push(parse_block(lines, at, child_indent)?);
            } else {
                items.push(Node::Scalar(String::new()));
            }
            continue;
        }
        if let Some(node) = alias(&rest).or_else(|| flow(&rest)) {
            items.push(node);
            continue;
        }
        match split_key(&rest) {
            // `- target: 80` opens a mapping whose remaining keys are
            // indented to where `target` starts.
            Some((key, value)) => {
                let mut entries = vec![(key, inline_value(lines, at, value, inner)?)];
                while let Some(next) = lines.get(*at) {
                    if next.indent != inner {
                        break;
                    }
                    let trimmed = next.text.trim_start();
                    if trimmed.starts_with('-') {
                        break;
                    }
                    let Some((key, value)) = split_key(trimmed) else {
                        break;
                    };
                    *at += 1;
                    entries.push((key, inline_value(lines, at, value, inner)?));
                }
                items.push(Node::Map(entries));
            }
            None => items.push(Node::Scalar(unquote(&rest))),
        }
    }
    Ok(Node::Seq(items))
}

fn parse_map(lines: &[Line], at: &mut usize, indent: usize) -> Result<Node> {
    let mut entries: Vec<(String, Node)> = Vec::new();
    while let Some(line) = lines.get(*at) {
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            // A deeper line with no key above it is not something pando can
            // place; skipping keeps one odd block from losing the file.
            *at += 1;
            continue;
        }
        let trimmed = line.text.trim_start();
        if trimmed.starts_with('-') {
            break;
        }
        let Some((key, value)) = split_key(trimmed) else {
            *at += 1;
            continue;
        };
        *at += 1;
        let node = inline_value(lines, at, value, indent)?;
        entries.push((key, node));
    }
    Ok(Node::Map(entries))
}

/// What follows `key:` — on the same line, or the block indented under it.
///
/// `volumes: &data` with the list under it is that list: an anchor is only
/// a label, and reading it as the text `&data` would lose the block.
fn inline_value(lines: &[Line], at: &mut usize, value: String, indent: usize) -> Result<Node> {
    let value = without_anchor(&value);
    if !value.is_empty() {
        return Ok(one_line(value));
    }
    let Some(next) = lines.get(*at) else {
        return Ok(Node::Scalar(String::new()));
    };
    // A sequence may sit at the parent's own column, which is legal YAML
    // and is how most compose files write `ports:`.
    let is_seq = next.text.trim_start().starts_with('-');
    if next.indent > indent || (is_seq && next.indent == indent) {
        return parse_block(lines, at, next.indent);
    }
    Ok(Node::Scalar(String::new()))
}

/// `key: value` with the key unquoted, or `None` when the line is not a
/// mapping entry at all.
fn split_key(text: &str) -> Option<(String, String)> {
    let mut quote: Option<char> = None;
    for (i, c) in text.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == ':' => {
                let after = &text[i + 1..];
                // `image: redis:7` — the separator is a colon followed by a
                // space or the end of the line, never one inside a value.
                if !after.is_empty() && !after.starts_with(' ') {
                    return None;
                }
                return Some((unquote(&text[..i]), after.trim().to_string()));
            }
            None => {}
        }
    }
    None
}

/// A value written on one line: an alias, a flow collection, or a scalar.
fn one_line(text: &str) -> Node {
    let text = without_anchor(text);
    alias(text)
        .or_else(|| flow(text))
        .unwrap_or_else(|| Node::Scalar(unquote(text)))
}

/// `&name` at the front of a value, dropped. What follows it — on the line,
/// or in the block under it — is the value.
fn without_anchor(text: &str) -> &str {
    let text = text.trim_start();
    if !text.starts_with('&') {
        return text;
    }
    match text.find(char::is_whitespace) {
        Some(end) => text[end..].trim_start(),
        None => "",
    }
}

/// `*name`, unquoted. A plain YAML scalar cannot start with `*`, so this
/// is never an image or a path.
fn alias(text: &str) -> Option<Node> {
    text.trim()
        .strip_prefix('*')
        .map(|name| Node::Alias(name.to_string()))
}

/// `[a, b]` and `{a: b}` on one line.
fn flow(text: &str) -> Option<Node> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        return Some(Node::Seq(
            split_flow(inner)
                .into_iter()
                .map(|item| one_line(&item))
                .collect(),
        ));
    }
    if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        let mut entries = Vec::new();
        for item in split_flow(inner) {
            if let Some((key, value)) = split_key(&item) {
                entries.push((key, one_line(&value)));
            }
        }
        return Some(Node::Map(entries));
    }
    None
}

/// Commas at nesting depth zero, outside quotes.
fn split_flow(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for c in text.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                current.push(c);
            }
            Some(_) => current.push(c),
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    current.push(c);
                }
                '[' | '{' => {
                    depth += 1;
                    current.push(c);
                }
                ']' | '}' => {
                    depth -= 1;
                    current.push(c);
                }
                ',' if depth == 0 => {
                    out.push(current.trim().to_string());
                    current = String::new();
                }
                _ => current.push(c),
            },
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out.retain(|item| !item.is_empty());
    out
}

fn unquote(text: &str) -> String {
    let text = text.trim();
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return text[1..text.len() - 1].to_string();
        }
    }
    text.to_string()
}
