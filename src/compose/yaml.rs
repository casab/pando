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
    let (node, unplaced) = parse_document(text)?;
    let Node::Map(top) = node else {
        bail!("a compose file is a mapping at its top level");
    };
    let mut file = ComposeFile::default();
    // Only under the two keys this reader takes anything from: an alias in
    // an `x-` block that nothing here reads changes nothing pando decides.
    let read = || {
        top.iter()
            .filter(|(key, _)| matches!(key.as_str(), "services" | "volumes"))
            .map(|(_, value)| value)
    };
    file.unresolved.aliases = read().any(refers_elsewhere);
    // Compose merges every document of a file into the first, so a second
    // one can add anything to a service, a bind mount among it, and a line
    // at the top level with no key in it may be one compose reads as part
    // of the value above it.
    file.unresolved.unread = unplaced
        || top.iter().any(|(key, value)| match (key.as_str(), value) {
            ("services", Node::Map(services)) => {
                services.iter().any(|(_, body)| service_unread(body))
            }
            ("services" | "volumes", value) => holds_unread(value),
            (_, value) => holds_spilled(value),
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
        Node::Scalar(_) | Node::Unread | Node::Spilled => false,
        Node::Seq(items) => items.iter().any(refers_elsewhere),
        Node::Map(fields) => fields
            .iter()
            .any(|(key, value)| key == "<<" || refers_elsewhere(value)),
    }
}

/// Whether anything under `node` is a value this reader could not read.
fn holds_unread(node: &Node) -> bool {
    match node {
        Node::Unread | Node::Spilled => true,
        Node::Scalar(_) | Node::Alias(_) => false,
        Node::Seq(items) => items.iter().any(holds_unread),
        Node::Map(fields) => fields.iter().any(|(_, value)| holds_unread(value)),
    }
}

/// Whether anything under `node` is a value that went on over lines this
/// reader takes for keys of its own (see [`Node::Spilled`]).
fn holds_spilled(node: &Node) -> bool {
    match node {
        Node::Spilled => true,
        Node::Scalar(_) | Node::Alias(_) | Node::Unread => false,
        Node::Seq(items) => items.iter().any(holds_spilled),
        Node::Map(fields) => fields.iter().any(|(_, value)| holds_spilled(value)),
    }
}

/// Whether what [`service`] reads of a service body is a value this reader
/// could not read: the body itself, anything under `ports:`, `volumes:`
/// and `depends_on:`, or the `image:`, `build:` (and its `context:`),
/// `container_name:` or `healthcheck:` (and its `disable:`) value.
///
/// Nothing else counts, unless it spilled over the keys after it. A
/// `command:` script written as a block scalar holds no port and no mount,
/// and a healthcheck's `test:` written as text is a check whatever the
/// text says; counting either doubted a file read whole.
fn service_unread(body: &Node) -> bool {
    let Node::Map(fields) = body else {
        return holds_unread(body);
    };
    let unread = |node: &Node| *node == Node::Unread;
    let unread_under = |fields: &[(String, Node)], want: &str| {
        fields
            .iter()
            .any(|(key, value)| key == want && unread(value))
    };
    holds_spilled(body)
        || fields
            .iter()
            .any(|(key, value)| match (key.as_str(), value) {
                ("ports" | "volumes" | "depends_on", value) => holds_unread(value),
                ("build", Node::Map(build)) => unread_under(build, "context"),
                ("healthcheck", Node::Map(check)) => unread_under(check, "disable"),
                ("image" | "build" | "container_name" | "healthcheck", value) => unread(value),
                _ => false,
            })
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
/// An alias stands for a block this reader never sees, and counts, and so
/// does a block it could not read.
fn declares_healthcheck(node: &Node) -> bool {
    let fields = match node {
        Node::Map(fields) => fields,
        Node::Alias(_) | Node::Unread | Node::Spilled => return true,
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
            "driver" => out.driver = value.scalar().map(str::to_string),
            "driver_opts" => {
                let Node::Map(opts) = value else { continue };
                out.driver_opts = opts
                    .iter()
                    .map(|(key, value)| (key.clone(), value.scalar().unwrap_or("").to_string()))
                    .collect();
            }
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
        Node::Seq(_) | Node::Alias(_) | Node::Unread | Node::Spilled => None,
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
        Node::Seq(_) | Node::Alias(_) | Node::Unread | Node::Spilled => None,
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
    /// A value this reader could not read (see [`unread`], [`value_on`]
    /// and [`parse_map`]). Nothing in it is taken for a port or a mount,
    /// and where this reader reads it the file says it was not read whole.
    Unread,
    /// An unread flow collection or quoted scalar that goes on over a line
    /// no deeper than its own key, which compose takes as part of it and
    /// this reader as a key of its own. The keys after it may be misread
    /// too, so it says the file was not read whole wherever it sits.
    Spilled,
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

/// The file's first YAML document, and whether the file holds text this
/// reader does not place in it: another document after it, or a line at
/// its top level with no key in it.
///
/// The top level is a mapping of every key read around such a line, not
/// one this reader could not read: a compose file is a mapping there, and
/// the parse failed saying it was none, where compose reads a flow value
/// that goes on over a line at column 0 as part of it.
fn parse_document(text: &str) -> Result<(Node, bool)> {
    let (lines, more) = significant_lines(text);
    let Some(first) = lines.first() else {
        return Ok((Node::Map(Vec::new()), more));
    };
    let mut at = 0usize;
    let start = first.text.trim_start();
    if start.starts_with('-') || opens_a_value(start) {
        return Ok((parse_block(&lines, &mut at, first.indent)?, more));
    }
    let (entries, keyless) = map_entries(&lines, &mut at, first.indent)?;
    Ok((Node::Map(entries), more || keyless))
}

/// The lines of the file's first YAML document, comments and blank lines
/// removed, tabs refused, and whether another document follows it.
///
/// A byte-order mark, `%` directives and the `---` that starts the
/// document are not part of it. Kept, `---` ended the top-level mapping
/// before its first key and the file read as declaring no services, and
/// a mark made its first key `\u{feff}services`. A `---` or `...` after
/// the document's first line ends it.
///
/// YAML forbids a tab as indentation, and a compose file indented with one
/// is a file docker itself will not read; saying so beats parsing it into
/// a shape that silently loses a service.
fn significant_lines(text: &str) -> (Vec<Line>, bool) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = Vec::new();
    let mut ended = false;
    for raw in text.lines() {
        let without_comment = strip_comment(raw);
        if without_comment.trim().is_empty() {
            continue;
        }
        let indent = without_comment.len() - without_comment.trim_start().len();
        let text = without_comment.trim_end();
        if indent == 0 && matches!(text, "---" | "...") {
            ended = !out.is_empty();
            continue;
        }
        if indent == 0 && text.starts_with('%') && out.is_empty() {
            continue;
        }
        if ended {
            return (out, true);
        }
        out.push(Line {
            indent,
            text: text.to_string(),
        });
    }
    (out, false)
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

/// A block at `indent`: a sequence of `-` entries, a mapping, or a value
/// that starts on the line under its key.
///
/// `volumes:` with `["./data:/x"]` on the next line is that list, and a
/// `{` or a tag there is not the first key of a mapping: read as one, the
/// bind mount inside it was lost without a trace.
fn parse_block(lines: &[Line], at: &mut usize, indent: usize) -> Result<Node> {
    let Some(first) = lines.get(*at) else {
        return parse_map(lines, at, indent);
    };
    let text = first.text.trim_start();
    if text.starts_with("- ") || text == "-" {
        return parse_seq(lines, at, indent);
    }
    if opens_a_value(text) {
        *at += 1;
        return Ok(value_on(lines, *at, text, indent));
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
        // A line under an entry, or one closing its brackets, goes on with
        // that entry's value. Ending the sequence there left the entries
        // after it to the mapping above, which ended at the first of them
        // and lost every key after it.
        if line.indent > indent || closes_only(trimmed) {
            *at += 1;
            continue;
        }
        if !trimmed.starts_with('-') {
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
        // `- {type: bind,` is a flow mapping still open, not a key.
        let entry = if opens_a_value(&rest) {
            None
        } else {
            split_key(&rest)
        };
        match entry {
            // `- target: 80` opens a mapping whose remaining keys are
            // indented to where `target` starts, and are read as any
            // mapping's are.
            Some((key, value)) => {
                let first = (key, inline_value(lines, at, value, inner)?);
                items.push(match parse_map(lines, at, inner)? {
                    Node::Map(mut entries) => {
                        entries.insert(0, first);
                        Node::Map(entries)
                    }
                    unread => unread,
                });
            }
            None => items.push(value_on(lines, *at, &rest, indent)),
        }
    }
    Ok(Node::Seq(items))
}

/// The mapping at `indent`, or one this reader could not read when a line
/// at its own column has no key in it (see [`map_entries`]).
fn parse_map(lines: &[Line], at: &mut usize, indent: usize) -> Result<Node> {
    let (entries, keyless) = map_entries(lines, at, indent)?;
    Ok(if keyless {
        Node::Unread
    } else {
        Node::Map(entries)
    })
}

/// The entries of the mapping at `indent`, and whether a line at its own
/// column had no key in it.
///
/// Such a line, other than one closing the brackets of the value above,
/// is one this reader cannot place: a plain scalar written on the line
/// under its key or `-`, an explicit `? key`, or a flow value going on
/// over its key's column. It is skipped and the keys after it are read,
/// but the mapping is not the one compose reads: `-` over
/// `./pgdata:/var/lib/postgresql/data` read as a mapping of nothing, a
/// mount with no source, where compose binds `./pgdata`.
fn map_entries(
    lines: &[Line],
    at: &mut usize,
    indent: usize,
) -> Result<(Vec<(String, Node)>, bool)> {
    let mut entries: Vec<(String, Node)> = Vec::new();
    let mut keyless = false;
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
        *at += 1;
        match split_key(trimmed) {
            Some((key, value)) => {
                let node = inline_value(lines, at, value, indent)?;
                entries.push((key, node));
            }
            None => keyless |= !closes_only(trimmed),
        }
    }
    Ok((entries, keyless))
}

/// What follows `key:` — on the same line, or the block indented under it.
///
/// `volumes: &data` with the list under it is that list: an anchor is only
/// a label, and reading it as the text `&data` would lose the block.
fn inline_value(lines: &[Line], at: &mut usize, value: String, indent: usize) -> Result<Node> {
    let value = without_anchor(&value);
    if !value.is_empty() {
        return Ok(value_on(lines, *at, value, indent));
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

/// A value written on one line: an alias, a flow collection, a scalar, or
/// one this reader could not read from its line.
fn one_line(text: &str) -> Node {
    let text = without_anchor(text);
    alias(text)
        .or_else(|| flow(text))
        .or_else(|| unread(text))
        .unwrap_or_else(|| Node::Scalar(unquote(text)))
}

/// The value `text` on the line just read, the one before `at`, whose key
/// or `-` sits at `indent`.
///
/// A flow collection still open at the end of its line goes on over the
/// lines under it and is read as the one line they make, since a line
/// break between its entries is only a space: Prettier wraps a flow list
/// too long for its line this way, and a healthcheck's `test:` wrapped
/// over `"NONE"` is one the file turns off. Every block here skips the
/// lines deeper than its own column and a line of closing brackets at it,
/// so what follows the value is read as compose reads it.
///
/// A quoted scalar that goes on over a line break, alone or inside a flow
/// collection, is not read: the break folds by rules of its own, and a `#`
/// on the line after it was taken for a comment. Nor is a flow collection
/// on several lines with a quote character in it that neither opens nor
/// closes a quoted scalar (see [`Flow::stray_quote`]): a comment kept after
/// `it's` was read as the start of the key under it. Going on only over
/// such lines, either is [`Node::Unread`]. Compose also takes a flow
/// collection or a quoted scalar that goes on over a line no deeper than
/// its key, which this reader would read as a key of its own; that one is
/// [`Node::Spilled`].
///
/// Any other value that goes on over the lines under it — a plain scalar
/// folded onto them, a block scalar's text — is [`Node::Unread`] too. Read
/// as its first line, `- ./pgdata` over `:/var/lib/postgresql/data` was a
/// mount with no source, where compose binds `./pgdata`.
fn value_on(lines: &[Line], at: usize, text: &str, indent: usize) -> Node {
    let start = without_properties(text);
    if !start.starts_with(['[', '{', '"', '\'']) {
        if lines.get(at).is_some_and(|next| next.indent > indent) {
            return Node::Unread;
        }
        return one_line(text);
    }
    let mut flow = Flow::default();
    flow.read(start);
    if !flow.open() {
        return one_line(text);
    }
    let mut quote_broken = flow.quote.is_some();
    let mut whole = text.trim().to_string();
    for line in &lines[at..] {
        if line.indent < indent || (line.indent == indent && !closes_only(&line.text)) {
            return Node::Spilled;
        }
        flow.read(&line.text);
        whole.push(' ');
        whole.push_str(line.text.trim());
        if !flow.open() {
            return if quote_broken || flow.stray_quote {
                Node::Unread
            } else {
                one_line(&whole)
            };
        }
        quote_broken |= flow.quote.is_some();
    }
    // Never closed, which compose does not read either.
    Node::Spilled
}

/// How far a flow collection or quoted scalar read so far is from closed.
#[derive(Default)]
struct Flow {
    /// `[` and `{` not yet closed.
    depth: usize,
    /// The quote a scalar is still open in.
    quote: Option<char>,
    /// Whether a `]` or `}` closed one that was never opened.
    overclosed: bool,
    /// Whether a quote character was read that neither opens nor closes a
    /// quoted scalar: the `'` in `it's`, an escaped `\"`, a doubled `''`,
    /// or one kind inside a scalar quoted with the other. The comment
    /// stripper takes some of these for quotes, so a `#` after one on the
    /// same line may be text this reader kept.
    stray_quote: bool,
}

impl Flow {
    fn open(&self) -> bool {
        self.depth > 0 || self.quote.is_some()
    }

    /// Reads on through one more line of it, and returns where in the line
    /// a `,` sits outside every bracket and quote. A quote starts a scalar
    /// only where a token does, so the `'` in `[it's]` opens nothing, and
    /// neither `\"` in a double-quoted scalar nor `''` in a single-quoted
    /// one closes it.
    fn read(&mut self, text: &str) -> Vec<usize> {
        let mut commas = Vec::new();
        let mut chars = text.char_indices().peekable();
        let mut previous = ' ';
        while let Some((at, c)) = chars.next() {
            match self.quote {
                Some('"') if c == '\\' => {
                    self.stray_quote |= chars.next().is_some_and(|(_, c)| matches!(c, '"' | '\''));
                }
                Some('\'') if c == '\'' && chars.peek().is_some_and(|&(_, c)| c == '\'') => {
                    chars.next();
                    self.stray_quote = true;
                }
                Some(quote) if c == quote => self.quote = None,
                Some(_) => self.stray_quote |= matches!(c, '"' | '\''),
                None => match c {
                    '"' | '\'' if previous.is_whitespace() || "[{,:".contains(previous) => {
                        self.quote = Some(c)
                    }
                    '"' | '\'' => self.stray_quote = true,
                    '[' | '{' => self.depth += 1,
                    ']' | '}' if self.depth == 0 => self.overclosed = true,
                    ']' | '}' => self.depth -= 1,
                    ',' if self.depth == 0 => commas.push(at),
                    _ => {}
                },
            }
            previous = c;
        }
        commas
    }
}

/// Whether a line holds nothing but brackets closing a flow collection.
fn closes_only(text: &str) -> bool {
    text.trim()
        .chars()
        .all(|c| matches!(c, ']' | '}' | ',' | ' '))
}

/// `text` without the anchor and the tag in front of it, if any: where the
/// value itself starts.
fn without_properties(text: &str) -> &str {
    let mut text = text.trim_start();
    while text.starts_with(['&', '!']) {
        text = match text.split_once(char::is_whitespace) {
            Some((_, rest)) => rest.trim_start(),
            None => "",
        };
    }
    text
}

/// Whether `text` starts a value rather than a mapping entry: an alias, a
/// flow collection, a tag or a block scalar, none of which a compose file
/// writes as a key.
fn opens_a_value(text: &str) -> bool {
    text.trim_start()
        .starts_with(['*', '[', '{', '!', '|', '>'])
}

/// A value this reader cannot take from its one line: a tag (`!override`,
/// `!reset`), which changes what the value after it means, a block scalar
/// (`|`, `>-`), whose text is on the lines under it, or a flow collection
/// or a quoted scalar closed only on a later line.
///
/// Read as the text on its line, each of these came out as a mount with no
/// source, an anonymous volume that is isolated already, whatever bind
/// mount the lines after it held. Those lines are still not read, but the
/// file now says so. A plain scalar cannot start with `|` or `>`, so a
/// block scalar is never an image or a path.
fn unread(text: &str) -> Option<Node> {
    let text = text.trim();
    let open = |start: char, end: char| {
        text.starts_with(start) && (text.len() < 2 || !text.ends_with(end))
    };
    let unclosed = open('[', ']') || open('{', '}') || open('"', '"') || open('\'', '\'');
    (text.starts_with(['!', '|', '>']) || unclosed).then_some(Node::Unread)
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

/// `[a, b]` and `{a: b}` on one line, or one this reader could not read
/// when its brackets and quotes do not close where it does.
fn flow(text: &str) -> Option<Node> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        let Some(items) = split_flow(inner) else {
            return Some(Node::Unread);
        };
        return Some(Node::Seq(items.into_iter().map(one_line).collect()));
    }
    if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        let Some(items) = split_flow(inner) else {
            return Some(Node::Unread);
        };
        let mut entries = Vec::new();
        for item in items {
            // `{"type":"bind"}` has keys compose reads and this reader
            // does not split, and a mapping read without them lost the
            // bind mount they made.
            let Some((key, value)) = split_key(item) else {
                return Some(Node::Unread);
            };
            entries.push((key, one_line(&value)));
        }
        return Some(Node::Map(entries));
    }
    None
}

/// The entries inside a flow collection's brackets, split at the commas
/// [`Flow`] finds between them, or `None` when that reading leaves a
/// bracket or a quote open, or closes a bracket the text never opened: the
/// brackets around it do not close where the collection does.
///
/// A scanner of its own took the `'` in `it's` for a quote, and every
/// entry after it went into the one before, `volumes:` among them.
fn split_flow(text: &str) -> Option<Vec<&str>> {
    let mut flow = Flow::default();
    let commas = flow.read(text);
    if flow.open() || flow.overclosed {
        return None;
    }
    let mut start = 0;
    let mut out = Vec::new();
    for end in commas.into_iter().chain([text.len()]) {
        let item = text[start..end].trim();
        if !item.is_empty() {
            out.push(item);
        }
        start = end + 1;
    }
    Some(out)
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
