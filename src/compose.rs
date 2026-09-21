//! Reading the project's own compose file.
//!
//! "Keep the project boring": a team that already describes its services in
//! a compose file should not have to describe them again for pando. So this
//! module reads that file and nothing else — it never writes to it, and the
//! per-worktree override it generates in the next slice lives under pando's
//! home.
//!
//! The parser is a deliberate subset of YAML rather than a dependency. Two
//! reasons. The override pando writes needs the `!override` and `!reset`
//! tags, which no serde YAML crate emits, so the *writing* side is
//! hand-rolled whatever happens. And what pando reads is a handful of keys
//! — `image`, `ports`, `volumes`, `container_name`, `healthcheck`,
//! `depends_on` — whose shapes are fixed by the compose specification.
//! Anything it does not understand is ignored rather than guessed at, and
//! a service pando cannot describe is refused by name rather than started
//! wrong.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The file names compose itself looks for, in its own order of
/// precedence. Detection offers the first one that exists.
pub const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// Container ports for images pando knows, for a service whose compose
/// entry publishes nothing.
///
/// A service with no `ports` is one the project reaches over the compose
/// network, where nothing needs publishing. An isolated worktree runs its
/// processes on the host, so the port has to be published — and this table
/// is the only way to know which one, short of pulling the image and
/// reading its `EXPOSE`. A service that is in neither the file nor this
/// table is refused for isolation by name; guessing would publish the
/// wrong port and fail much later, inside the app.
///
/// Matched against the image name with the tag stripped, by the last path
/// segment, so `postgres:16`, `library/postgres`, and
/// `public.ecr.aws/docker/library/postgres:16-alpine` all match `postgres`.
const IMAGE_PORTS: [(&str, &[u16]); 12] = [
    ("postgres", &[5432]),
    ("postgis", &[5432]),
    ("mysql", &[3306]),
    ("mariadb", &[3306]),
    ("redis", &[6379]),
    ("valkey", &[6379]),
    ("mongo", &[27017]),
    ("mailpit", &[8025, 1025]),
    ("mailhog", &[8025, 1025]),
    ("elasticsearch", &[9200]),
    ("rabbitmq", &[5672, 15672]),
    ("minio", &[9000, 9001]),
];

/// The compose file as much of it as pando needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposeFile {
    pub services: BTreeMap<String, Service>,
    /// The top-level `volumes:` block. Only the keys that decide whether
    /// the compose project name isolates a volume are kept.
    pub volumes: BTreeMap<String, TopVolume>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Service {
    pub image: Option<String>,
    /// Every `ports:` entry, in file order.
    pub ports: Vec<Port>,
    pub volumes: Vec<Mount>,
    pub container_name: Option<String>,
    /// Whether the service declares a `healthcheck`. Readiness prefers it:
    /// a connect succeeding says the socket is open, not that the database
    /// will answer a query.
    pub healthcheck: bool,
    pub depends_on: Vec<String>,
}

/// One `ports:` entry. `published` is what the *project* asked for, which
/// an isolated worktree replaces; `container` is the one that survives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub container: u16,
    pub published: Option<u16>,
    pub host: Option<String>,
}

/// One `volumes:` entry of a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// A named volume, which the compose project name prefixes — which is
    /// what makes a per-worktree project isolate the data for free.
    Named(String),
    /// A host path. One relative to the compose file lands *inside the
    /// repository*, which Invariant 1 forbids pando to write into.
    Bind(String),
    /// `- /var/lib/postgresql/data` with no source: docker makes one up,
    /// and it is per-container, so it is isolated already.
    Anonymous,
}

/// A top-level volume declaration. Both of these defeat the project-name
/// prefix, so a service using one is refused for isolation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopVolume {
    /// An explicit `name:`, which compose uses verbatim rather than
    /// prefixing.
    pub name: Option<String>,
    /// `external: true`: the volume is expected to exist already, and
    /// every worktree would share the one volume.
    pub external: bool,
}

impl Service {
    /// The container port an isolated worktree publishes: the first one the
    /// file declares, else the first one the image table knows.
    ///
    /// One port per service, deliberately. A role is one name and one
    /// number, and a second published port would need a second role and a
    /// second allocation with no name to give it. A service whose extra
    /// ports matter — mailpit's web UI beside its SMTP port — gets the
    /// first of them; the rest stay inside the compose network.
    pub fn container_port(&self) -> Option<u16> {
        if let Some(port) = self.ports.first() {
            return Some(port.container);
        }
        self.image
            .as_deref()
            .and_then(image_ports)
            .and_then(|ports| ports.first().copied())
    }
}

/// The container ports pando knows for an image, by its last path segment
/// with any tag or digest stripped.
pub fn image_ports(image: &str) -> Option<&'static [u16]> {
    let name = image_name(image);
    IMAGE_PORTS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, ports)| *ports)
}

/// `public.ecr.aws/docker/library/postgres:16-alpine` becomes `postgres`.
fn image_name(image: &str) -> &str {
    let image = image.split('@').next().unwrap_or(image);
    let last = image.rsplit('/').next().unwrap_or(image);
    last.split(':').next().unwrap_or(last)
}

/// The compose file `root` declares, if any, in compose's own precedence
/// order. Returned relative to `root`, which is the form config holds.
pub fn find(root: &Path) -> Option<String> {
    COMPOSE_FILES
        .iter()
        .find(|name| root.join(name).is_file())
        .map(|name| (*name).to_string())
}

// ---- the per-worktree override -------------------------------------------

/// The compose project name for one worktree: what isolates its
/// containers, its network, and — because compose prefixes named volumes
/// with it — its data.
///
/// Compose allows `[a-z0-9][a-z0-9_-]*`, and a worktree directory is named
/// after a branch, so `feat+checkout` has to be folded. Folding can
/// collide: `feat+one` and `feat-one` are two worktrees and would become
/// one set of containers and one database. So a name that had to change
/// carries four hex characters of its own hash, and a name that did not is
/// left readable.
pub fn project_name(project_id: &str, worktree: &str) -> String {
    let project = fold(project_id);
    let name = fold(worktree);
    if name == worktree {
        return format!("pando-{project}-{name}");
    }
    let digest = format!("{:x}", md5::compute(worktree.as_bytes()));
    format!("pando-{project}-{name}-{}", &digest[..4])
}

fn fold(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect()
}

/// One included service, resolved: the port inside the container, and the
/// port on the host pando allocated for this worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub service: String,
    pub container: u16,
    pub host: u16,
}

/// The container port of every included service, or the reason this
/// worktree cannot be isolated.
///
/// Every refusal names the service and says what to change in the
/// project's own file, because that is the only place it can be fixed:
/// Invariant 1 forbids pando to edit a compose file, and rewriting a bind
/// mount underneath the developer would put the isolated database
/// somewhere they did not ask for.
pub fn resolve_included(file: &ComposeFile, include: &[String]) -> Result<Vec<(String, u16)>> {
    let mut out = Vec::new();
    for name in include {
        let service = file.services.get(name).with_context(|| {
            let known: Vec<&str> = file.services.keys().map(String::as_str).collect();
            format!(
                "the compose file has no service named {name:?} — it declares: {}",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            )
        })?;
        check_mounts(name, service, file)?;
        check_depends_on(name, service, include)?;
        let port = service.container_port().with_context(|| {
            format!(
                "service {name:?} publishes no port and its image ({}) is not one pando knows a \
                 port for — add a `ports:` entry to it in the compose file, or drop it from \
                 `include`",
                service.image.as_deref().unwrap_or("none")
            )
        })?;
        out.push((name.clone(), port));
    }
    Ok(out)
}

/// A bind mount relative to the compose file lands inside the worktree,
/// and a named volume the project pins by name or borrows from outside is
/// one every worktree would share.
fn check_mounts(name: &str, service: &Service, file: &ComposeFile) -> Result<()> {
    for mount in &service.volumes {
        match mount {
            Mount::Bind(source) if source.starts_with('/') || source.starts_with('~') => {}
            Mount::Bind(source) if source.starts_with('$') => bail!(
                "service {name:?} mounts {source:?}, and pando cannot tell whether that is \
                 inside the repository — isolating it could write into your worktree. Use a \
                 named volume for it, or drop {name:?} from `include`"
            ),
            Mount::Bind(source) => bail!(
                "service {name:?} mounts {source:?}, which is inside the repository — an \
                 isolated copy would write its data into your worktree, which pando never \
                 does. Change it to a named volume in the compose file, or drop {name:?} \
                 from `include`"
            ),
            Mount::Named(volume) => {
                let Some(declared) = file.volumes.get(volume) else {
                    continue;
                };
                if declared.external {
                    bail!(
                        "service {name:?} uses the volume {volume:?}, which is declared \
                         `external: true` — compose does not prefix it with the project name, \
                         so every worktree would share one copy of that data"
                    );
                }
                if let Some(literal) = &declared.name {
                    bail!(
                        "service {name:?} uses the volume {volume:?}, which pins its name to \
                         {literal:?} — compose does not prefix a pinned name with the project \
                         name, so every worktree would share one copy of that data"
                    );
                }
            }
            Mount::Anonymous => {}
        }
    }
    Ok(())
}

/// `up -d <included>` starts anything an included service depends on, and
/// those come up on the ports the project hardcoded — which the second
/// worktree then collides with. Refusing is honest; `--no-deps` would
/// start a service whose dependency is missing.
fn check_depends_on(name: &str, service: &Service, include: &[String]) -> Result<()> {
    for needed in &service.depends_on {
        if !include.iter().any(|included| included == needed) {
            bail!(
                "service {name:?} depends_on {needed:?}, which is not in `include` — compose \
                 would start {needed:?} too, on the port the project hardcoded, and the second \
                 worktree would collide with the first. Add {needed:?} to `include`, or drop \
                 {name:?}"
            );
        }
    }
    Ok(())
}

/// The override file for one worktree: the published ports replaced, not
/// appended to, and any `container_name` removed.
///
/// `!override` because compose *merges* `ports` lists across files, which
/// would keep the project's own hardcoded port beside the allocated one
/// and collide across worktrees the moment a second one started.
/// `container_name: !reset` because a fixed container name is unique per
/// daemon, so the second worktree's `up` would fail on it.
pub fn render_override(worktree: &str, published: &[Published]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# generated by pando for the worktree {worktree}\n\
         # regenerated on every isolated start; edits here are lost\n\
         # the project's own compose file is never modified\n"
    ));
    if published.is_empty() {
        out.push_str("services: {}\n");
        return out;
    }
    let mut sorted: Vec<&Published> = published.iter().collect();
    sorted.sort_by(|a, b| a.service.cmp(&b.service));
    out.push_str("services:\n");
    for entry in sorted {
        out.push_str(&format!("  {}:\n", entry.service));
        out.push_str(&format!(
            "    ports: !override [\"127.0.0.1:{}:{}\"]\n",
            entry.host, entry.container
        ));
        out.push_str("    container_name: !reset\n");
    }
    out
}

/// Reads and parses a compose file, naming the file in any error.
pub fn read(path: &Path) -> Result<ComposeFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the compose file {}", path.display()))?;
    parse(&text).with_context(|| format!("in {}", path.display()))
}

/// Where a service's compose file lives inside a worktree, refusing a
/// configured path that would climb out of it.
///
/// `file` comes from config, and config is a file a human edits: an
/// absolute path or a `..` would have pando reading — and, through the
/// project directory compose derives from it, *writing bind mounts* —
/// somewhere it does not own.
pub fn file_in(worktree: &Path, file: &str) -> Result<PathBuf> {
    use std::path::Component;
    let relative = Path::new(file);
    if file.trim().is_empty() {
        bail!("a compose service needs a `file`");
    }
    if relative.is_absolute() {
        bail!("compose file {file:?} must be relative to the worktree");
    }
    if relative
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        bail!("compose file {file:?} must not escape the worktree");
    }
    Ok(worktree.join(relative))
}

// ---- parsing --------------------------------------------------------------

pub fn parse(text: &str) -> Result<ComposeFile> {
    let node = parse_document(text)?;
    let Node::Map(top) = node else {
        bail!("a compose file is a mapping at its top level");
    };
    let mut file = ComposeFile::default();
    for (key, value) in &top {
        match key.as_str() {
            "services" => {
                let Node::Map(services) = value else { continue };
                for (name, body) in services {
                    file.services.insert(name.clone(), service(body));
                }
            }
            "volumes" => {
                let Node::Map(volumes) = value else { continue };
                for (name, body) in volumes {
                    file.volumes.insert(name.clone(), top_volume(body));
                }
            }
            _ => {}
        }
    }
    Ok(file)
}

fn service(node: &Node) -> Service {
    let mut out = Service::default();
    let Node::Map(fields) = node else {
        return out;
    };
    for (key, value) in fields {
        match key.as_str() {
            "image" => out.image = value.scalar().map(str::to_string),
            "container_name" => out.container_name = value.scalar().map(str::to_string),
            // Only that it is there. What it runs is docker's business;
            // pando asks `docker compose ps` whether it passed.
            "healthcheck" => out.healthcheck = true,
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
        Node::Seq(_) => None,
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
fn first_of_range(text: &str) -> Option<u16> {
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
        Node::Seq(_) => None,
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
        let rest = trimmed[1..].trim_start().to_string();
        // The column the entry's own content starts at, so `- target: 80`
        // followed by `  published: 8080` reads as one mapping.
        let inner = line.indent + (trimmed.len() - trimmed[1..].trim_start().len());
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
        if let Some(node) = flow(&rest) {
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
fn inline_value(lines: &[Line], at: &mut usize, value: String, indent: usize) -> Result<Node> {
    if !value.is_empty() {
        if let Some(node) = flow(&value) {
            return Ok(node);
        }
        return Ok(Node::Scalar(unquote(&value)));
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

/// `[a, b]` and `{a: b}` on one line.
fn flow(text: &str) -> Option<Node> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        return Some(Node::Seq(
            split_flow(inner)
                .into_iter()
                .map(|item| flow(&item).unwrap_or_else(|| Node::Scalar(unquote(&item))))
                .collect(),
        ));
    }
    if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        let mut entries = Vec::new();
        for item in split_flow(inner) {
            if let Some((key, value)) = split_key(&item) {
                entries.push((
                    key,
                    flow(&value).unwrap_or_else(|| Node::Scalar(unquote(&value))),
                ));
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

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_ONE: &str = r#"
services:
  postgres:
    image: postgres:16
    ports: ["5432:5432"]
  redis:
    image: redis:7
    ports: ["6379:6379"]
  mailpit:
    image: axllent/mailpit
    ports: ["1025:1025"]
"#;

    #[test]
    fn a_flow_ports_list_parses_into_published_and_container() {
        let file = parse(FIXTURE_ONE).unwrap();
        assert_eq!(
            file.services.keys().collect::<Vec<_>>(),
            vec!["mailpit", "postgres", "redis"]
        );
        let pg = &file.services["postgres"];
        assert_eq!(pg.image.as_deref(), Some("postgres:16"));
        assert_eq!(
            pg.ports,
            vec![Port {
                container: 5432,
                published: Some(5432),
                host: None
            }]
        );
        assert_eq!(pg.container_port(), Some(5432));
        assert!(!pg.healthcheck);
    }

    #[test]
    fn every_short_port_form_is_understood() {
        let text = r#"
services:
  a:
    ports:
      - "3000"
      - "8000:8000"
      - "127.0.0.1:8001:8002"
      - "9090-9091:8080-8081"
      - "6060:6060/udp"
      - "[::1]:70:71"
"#;
        let ports = &parse(text).unwrap().services["a"].ports;
        assert_eq!(
            ports,
            &vec![
                Port {
                    container: 3000,
                    published: None,
                    host: None
                },
                Port {
                    container: 8000,
                    published: Some(8000),
                    host: None
                },
                Port {
                    container: 8002,
                    published: Some(8001),
                    host: Some("127.0.0.1".into())
                },
                Port {
                    container: 8080,
                    published: Some(9090),
                    host: None
                },
                Port {
                    container: 6060,
                    published: Some(6060),
                    host: None
                },
                Port {
                    container: 71,
                    published: Some(70),
                    host: Some("[::1]".into())
                },
            ]
        );
    }

    #[test]
    fn the_long_port_form_is_understood() {
        let text = r#"
services:
  db:
    image: postgres:16
    ports:
      - name: sql
        target: 5432
        published: "15432"
        protocol: tcp
      - target: 5433
"#;
        let ports = &parse(text).unwrap().services["db"].ports;
        assert_eq!(
            ports,
            &vec![
                Port {
                    container: 5432,
                    published: Some(15432),
                    host: None
                },
                Port {
                    container: 5433,
                    published: None,
                    host: None
                },
            ]
        );
    }

    #[test]
    fn named_and_bind_volumes_are_told_apart() {
        let text = r#"
services:
  db:
    image: postgres:16
    volumes:
      - dbdata:/var/lib/postgresql/data
      - ./seed:/docker-entrypoint-initdb.d
      - /var/log
      - type: bind
        source: ./conf
        target: /etc/conf
      - type: volume
        source: extra
        target: /extra
volumes:
  dbdata:
  shared:
    name: literally-this
  borrowed:
    external: true
"#;
        let file = parse(text).unwrap();
        assert_eq!(
            file.services["db"].volumes,
            vec![
                Mount::Named("dbdata".into()),
                Mount::Bind("./seed".into()),
                Mount::Anonymous,
                Mount::Bind("./conf".into()),
                Mount::Named("extra".into()),
            ]
        );
        assert_eq!(file.volumes["dbdata"], TopVolume::default());
        assert_eq!(
            file.volumes["shared"].name.as_deref(),
            Some("literally-this")
        );
        assert!(file.volumes["borrowed"].external);
        assert!(!file.volumes["dbdata"].external);
    }

    #[test]
    fn container_name_healthcheck_and_depends_on_are_read() {
        let text = r#"
services:
  web:
    image: nginx
    container_name: my-web
    depends_on:
      - db
      - cache
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost"]
      interval: 10s
  api:
    image: node
    depends_on:
      db:
        condition: service_healthy
"#;
        let file = parse(text).unwrap();
        let web = &file.services["web"];
        assert_eq!(web.container_name.as_deref(), Some("my-web"));
        assert!(web.healthcheck);
        assert_eq!(web.depends_on, vec!["db", "cache"]);
        assert!(!file.services["api"].healthcheck);
        assert_eq!(file.services["api"].depends_on, vec!["db"]);
    }

    #[test]
    fn a_service_with_no_ports_falls_back_to_the_image_table() {
        let text =
            "services:\n  cache:\n    image: redis:7-alpine\n  odd:\n    image: acme/thing\n";
        let file = parse(text).unwrap();
        assert_eq!(file.services["cache"].container_port(), Some(6379));
        assert_eq!(
            file.services["odd"].container_port(),
            None,
            "an unknown image with no declared port cannot be published"
        );
    }

    #[test]
    fn an_image_is_matched_by_its_last_segment_without_its_tag() {
        assert_eq!(image_ports("postgres:16"), Some(&[5432u16][..]));
        assert_eq!(
            image_ports("public.ecr.aws/docker/library/postgres:16-alpine"),
            Some(&[5432u16][..])
        );
        assert_eq!(image_ports("axllent/mailpit"), Some(&[8025u16, 1025][..]));
        assert_eq!(image_ports("redis@sha256:abc"), Some(&[6379u16][..]));
        assert_eq!(image_ports("nginx"), None);
    }

    // The two older spellings, and the `version:` key compose now ignores.
    #[test]
    fn a_legacy_file_with_a_version_key_still_parses() {
        let text = "version: \"3.8\"\nservices:\n  db:\n    image: mysql:8\n";
        let file = parse(text).unwrap();
        assert_eq!(file.services["db"].container_port(), Some(3306));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored_but_not_inside_quotes() {
        let text =
            "# top\nservices:\n\n  db:\n    image: \"redis#7\"  # trailing\n    ports: [\"1:2\"]\n";
        let file = parse(text).unwrap();
        assert_eq!(file.services["db"].image.as_deref(), Some("redis#7"));
        assert_eq!(file.services["db"].ports.len(), 1);
    }

    #[test]
    fn a_flow_mapping_is_understood() {
        let text = "services:\n  db:\n    image: postgres\n    environment: { POSTGRES_PASSWORD: x }\n    ports: [ \"5432:5432\" ]\n";
        let file = parse(text).unwrap();
        assert_eq!(file.services["db"].ports.len(), 1);
    }

    #[test]
    fn a_file_with_no_services_is_not_an_error() {
        assert_eq!(parse("").unwrap(), ComposeFile::default());
        assert_eq!(parse("volumes:\n  a:\n").unwrap().services.len(), 0);
    }

    #[test]
    fn find_prefers_composes_own_order() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find(dir.path()), None);
        std::fs::write(dir.path().join("docker-compose.yml"), "services:\n").unwrap();
        assert_eq!(find(dir.path()).as_deref(), Some("docker-compose.yml"));
        std::fs::write(dir.path().join("compose.yaml"), "services:\n").unwrap();
        assert_eq!(find(dir.path()).as_deref(), Some("compose.yaml"));
    }

    #[test]
    fn a_compose_path_may_not_leave_the_worktree() {
        let worktree = Path::new("/tmp/wt");
        assert_eq!(
            file_in(worktree, "docker/compose.yml").unwrap(),
            PathBuf::from("/tmp/wt/docker/compose.yml")
        );
        for bad in ["/etc/compose.yml", "../compose.yml", "a/../../b.yml", "  "] {
            let err = file_in(worktree, bad).unwrap_err().to_string();
            assert!(
                err.contains("compose") || err.contains("file"),
                "{bad:?}: {err}"
            );
        }
    }

    // ---- the override ----------------------------------------------------

    #[test]
    fn a_project_name_is_readable_when_nothing_had_to_be_folded() {
        assert_eq!(
            project_name("acme-shop-3f9a2c1d", "main2"),
            "pando-acme-shop-3f9a2c1d-main2"
        );
    }

    #[test]
    fn a_folded_project_name_carries_a_hash_so_two_branches_cannot_collide() {
        let plus = project_name("acme-3f9a2c1d", "feat+one");
        let dash = project_name("acme-3f9a2c1d", "feat-one");
        assert!(plus.starts_with("pando-acme-3f9a2c1d-feat-one-"), "{plus}");
        assert_ne!(
            plus, dash,
            "two worktrees must never share one set of containers and volumes"
        );
        // Every character compose allows, and nothing else.
        assert!(
            plus.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "{plus}"
        );
        assert_eq!(plus, project_name("acme-3f9a2c1d", "feat+one"), "stable");
    }

    #[test]
    fn an_uppercase_branch_name_does_not_fold_onto_its_lowercase_twin() {
        assert_ne!(
            project_name("p-1", "Feat+One"),
            project_name("p-1", "feat+one")
        );
    }

    #[test]
    fn the_override_replaces_ports_and_clears_the_container_name() {
        let rendered = render_override(
            "feat+one",
            &[
                Published {
                    service: "redis".into(),
                    container: 6379,
                    host: 17_006,
                },
                Published {
                    service: "postgres".into(),
                    container: 5432,
                    host: 17_004,
                },
            ],
        );
        assert_eq!(
            rendered
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>(),
            vec![
                "services:",
                "  postgres:",
                "    ports: !override [\"127.0.0.1:17004:5432\"]",
                "    container_name: !reset",
                "  redis:",
                "    ports: !override [\"127.0.0.1:17006:6379\"]",
                "    container_name: !reset",
            ]
        );
        assert!(rendered.starts_with("# generated by pando for the worktree feat+one"));
    }

    #[test]
    fn an_override_with_nothing_included_is_still_a_valid_file() {
        assert!(render_override("feat+one", &[]).ends_with("services: {}\n"));
    }

    #[test]
    fn included_services_resolve_to_their_container_ports() {
        let file = parse(FIXTURE_ONE).unwrap();
        assert_eq!(
            resolve_included(&file, &["postgres".into(), "redis".into()]).unwrap(),
            vec![("postgres".to_string(), 5432), ("redis".to_string(), 6379)]
        );
    }

    #[test]
    fn a_service_the_compose_file_does_not_declare_is_refused_by_name() {
        let file = parse(FIXTURE_ONE).unwrap();
        let err = format!(
            "{:#}",
            resolve_included(&file, &["pgsql".into()]).unwrap_err()
        );
        assert!(err.contains("pgsql"), "{err}");
        assert!(
            err.contains("postgres, redis"),
            "it lists what is there: {err}"
        );
    }

    #[test]
    fn a_relative_bind_mount_is_refused_by_name() {
        let text = "services:\n  db:\n    image: postgres:16\n    volumes:\n      - ./data:/var/lib/postgresql/data\n";
        let file = parse(text).unwrap();
        let err = format!("{:#}", resolve_included(&file, &["db".into()]).unwrap_err());
        assert!(err.contains("\"db\""), "{err}");
        assert!(err.contains("./data"), "{err}");
        assert!(
            err.contains("named volume"),
            "it says what to change: {err}"
        );
    }

    #[test]
    fn an_absolute_bind_mount_is_allowed_because_it_is_not_in_the_repository() {
        let text = "services:\n  db:\n    image: postgres:16\n    volumes:\n      - /var/run/docker.sock:/var/run/docker.sock\n      - ~/caches:/caches\n";
        let file = parse(text).unwrap();
        assert!(resolve_included(&file, &["db".into()]).is_ok());
    }

    #[test]
    fn a_bind_mount_pando_cannot_place_is_refused_rather_than_guessed_at() {
        let text =
            "services:\n  db:\n    image: postgres:16\n    volumes:\n      - $PWD/data:/data\n";
        let file = parse(text).unwrap();
        let err = format!("{:#}", resolve_included(&file, &["db".into()]).unwrap_err());
        assert!(err.contains("cannot tell"), "{err}");
    }

    #[test]
    fn a_volume_the_project_name_does_not_isolate_is_refused_by_name() {
        for (tail, needle) in [
            ("volumes:\n  dbdata:\n    external: true\n", "external"),
            ("volumes:\n  dbdata:\n    name: shared-db\n", "shared-db"),
        ] {
            let text = format!(
                "services:\n  db:\n    image: postgres:16\n    volumes:\n      - dbdata:/var/lib/postgresql/data\n{tail}"
            );
            let file = parse(&text).unwrap();
            let err = format!("{:#}", resolve_included(&file, &["db".into()]).unwrap_err());
            assert!(err.contains("dbdata"), "{err}");
            assert!(err.contains(needle), "{err}");
            assert!(err.contains("share"), "it says why it matters: {err}");
        }
    }

    #[test]
    fn a_plain_named_volume_is_isolated_by_the_project_name_and_allowed() {
        let text = "services:\n  db:\n    image: postgres:16\n    volumes:\n      - dbdata:/var/lib/postgresql/data\nvolumes:\n  dbdata:\n";
        let file = parse(text).unwrap();
        assert_eq!(
            resolve_included(&file, &["db".into()]).unwrap(),
            vec![("db".to_string(), 5432)]
        );
    }

    #[test]
    fn a_dependency_left_out_of_include_is_refused_naming_both() {
        let text = "services:\n  api:\n    image: node\n    ports: [\"3000:3000\"]\n    depends_on:\n      - db\n  db:\n    image: postgres:16\n";
        let file = parse(text).unwrap();
        let err = format!(
            "{:#}",
            resolve_included(&file, &["api".into()]).unwrap_err()
        );
        assert!(err.contains("\"api\""), "{err}");
        assert!(err.contains("\"db\""), "{err}");
        // And it is fine once both are in.
        assert!(resolve_included(&file, &["api".into(), "db".into()]).is_ok());
    }

    #[test]
    fn an_unknown_image_with_no_declared_port_is_refused_by_name() {
        let text = "services:\n  odd:\n    image: acme/thing:1\n";
        let file = parse(text).unwrap();
        let err = format!(
            "{:#}",
            resolve_included(&file, &["odd".into()]).unwrap_err()
        );
        assert!(err.contains("odd"), "{err}");
        assert!(err.contains("acme/thing:1"), "it names the image: {err}");
        assert!(err.contains("ports:"), "it says what to add: {err}");
    }

    #[test]
    fn every_fixture_compose_file_parses() {
        // The shapes the fixture catalogue uses, so a parser change that
        // breaks one is caught here and not three phases later.
        let messy = "services:\n  db:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n  \
                     cache:\n    image: redis:7\n    ports: [\"6379:6379\"]\n  \
                     queue:\n    image: rabbitmq:3\n    ports: [\"5672:5672\"]\n  \
                     mail:\n    image: axllent/mailpit\n    ports: [\"1025:1025\"]\n";
        let file = parse(messy).unwrap();
        assert_eq!(file.services.len(), 4);
        assert_eq!(file.services["queue"].container_port(), Some(5672));
        assert_eq!(file.services["mail"].container_port(), Some(1025));
    }
}
