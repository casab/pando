//! Container images pando recognises in a compose file: what kind of
//! service each one is, which env keys tend to address it, and which
//! container ports it listens on.
//!
//! Matched by the image's last path segment with any tag or digest
//! stripped, so `postgres:16`, `library/postgres` and
//! `public.ecr.aws/docker/library/postgres:16-alpine` are all `postgres`.

/// Whether an application talks to this image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A database, cache, queue or store the app reads an address for.
    ///
    /// One that nothing in the env example points at is the ambiguous case:
    /// the developer may want a private copy of it and pando cannot tell, so
    /// it asks. An image that is not an app service — a mail catcher, a
    /// dashboard, something pando has never heard of — is left unticked
    /// without a question, because an app that never reads an address for
    /// it is not talking to it.
    App,
    /// An image an app usually does not address, so a missing key is not a
    /// question. A mail catcher is the classic: it exists so nothing leaves
    /// the machine, and half of them are never configured at all.
    Utility,
}

#[derive(Debug, Clone, Copy)]
pub struct Image {
    pub name: &'static str,
    pub role: Role,
    /// Env-key prefixes that name this service when the service's own name
    /// does not: `DATABASE_URL` for a service called `pg`.
    pub env_prefixes: &'static [&'static str],
    /// Container ports, for a service whose compose entry publishes
    /// nothing.
    ///
    /// A service with no `ports` is one the project reaches over the
    /// compose network, where nothing needs publishing. An isolated
    /// worktree runs its processes on the host, so the port has to be
    /// published — and this is the only way to know which one, short of
    /// pulling the image and reading its `EXPOSE`. Empty when pando does not
    /// know: such a service is refused for isolation by name, because
    /// guessing would publish the wrong port and fail much later, inside
    /// the app.
    pub ports: &'static [u16],
}

const fn app(
    name: &'static str,
    env_prefixes: &'static [&'static str],
    ports: &'static [u16],
) -> Image {
    Image {
        name,
        role: Role::App,
        env_prefixes,
        ports,
    }
}

const MAIL: &[&str] = &["SMTP", "MAIL", "MAILER"];

const fn utility(name: &'static str, ports: &'static [u16]) -> Image {
    Image {
        name,
        role: Role::Utility,
        env_prefixes: MAIL,
        ports,
    }
}

const POSTGRES: &[&str] = &["DATABASE", "DB", "POSTGRES", "PG", "POSTGRESQL"];

pub const IMAGES: [Image; 21] = [
    app("postgres", POSTGRES, &[5432]),
    app("postgis", &["DATABASE", "DB", "POSTGRES", "PG"], &[5432]),
    // Postgres with an extension built in, or packaged by somebody else:
    // `pgvector/pgvector`, `bitnami/postgresql`, `timescale/timescaledb`.
    app("pgvector", POSTGRES, &[5432]),
    app("postgresql", POSTGRES, &[5432]),
    app("timescaledb", POSTGRES, &[5432]),
    app("timescaledb-ha", POSTGRES, &[5432]),
    app("mysql", &["DATABASE", "DB", "MYSQL"], &[3306]),
    app("mariadb", &["DATABASE", "DB", "MYSQL", "MARIADB"], &[3306]),
    app("redis", &["REDIS", "CACHE"], &[6379]),
    app("redis-stack", &["REDIS", "CACHE"], &[6379]),
    app("redis-stack-server", &["REDIS", "CACHE"], &[6379]),
    app("valkey", &["REDIS", "VALKEY", "CACHE"], &[6379]),
    app("mongo", &["MONGO", "MONGODB", "DATABASE"], &[27017]),
    app(
        "elasticsearch",
        &["ELASTIC", "ELASTICSEARCH", "SEARCH"],
        &[9200],
    ),
    app(
        "rabbitmq",
        &["RABBITMQ", "AMQP", "QUEUE", "BROKER"],
        &[5672, 15672],
    ),
    app("kafka", &["KAFKA", "BROKER"], &[]),
    app("minio", &["MINIO", "S3", "STORAGE"], &[9000, 9001]),
    app("clickhouse", &["CLICKHOUSE"], &[]),
    utility("mailpit", &[8025, 1025]),
    utility("mailhog", &[8025, 1025]),
    utility("maildev", &[]),
];

/// `public.ecr.aws/docker/library/postgres:16-alpine` becomes `postgres`.
pub fn image_name(image: &str) -> &str {
    let image = image.split('@').next().unwrap_or(image);
    let last = image.rsplit('/').next().unwrap_or(image);
    last.split(':').next().unwrap_or(last)
}

/// The row for an image reference, if pando knows it.
pub fn known(image: &str) -> Option<&'static Image> {
    let name = image_name(image);
    IMAGES.iter().find(|known| known.name == name)
}

/// The container ports pando knows for an image.
pub fn ports(image: &str) -> Option<&'static [u16]> {
    known(image)
        .map(|image| image.ports)
        .filter(|ports| !ports.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_is_listed_once() {
        for (i, image) in IMAGES.iter().enumerate() {
            assert!(
                IMAGES[i + 1..].iter().all(|other| other.name != image.name),
                "{} is listed twice",
                image.name
            );
        }
    }

    #[test]
    fn a_reference_is_known_by_its_last_segment() {
        assert_eq!(
            known("library/postgres:16").map(|i| i.name),
            Some("postgres")
        );
        assert_eq!(ports("redis@sha256:abc"), Some(&[6379u16][..]));
        assert_eq!(ports("kafka"), None);
        assert!(known("nginx").is_none());
        assert_eq!(
            known("pgvector/pgvector:pg16").map(|i| i.name),
            Some("pgvector")
        );
        assert_eq!(ports("bitnami/postgresql:16"), Some(&[5432u16][..]));
        assert_eq!(
            ports("redis/redis-stack-server:latest"),
            Some(&[6379u16][..])
        );
    }
}
