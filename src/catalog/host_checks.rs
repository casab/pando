//! How dev servers refuse a request for a host they do not know, and the
//! one line in the project that lets a public URL's host in.
//!
//! A share hands the dev server the visitor's `Host`, which is the
//! tunnel's own hostname, and several frameworks' dev servers answer every
//! host that is not `localhost` or an address with an error page of their
//! own. pando does not edit framework config, so a refusal is reported
//! with the change to make.

/// One dev server's refusal, told apart by its answer.
#[derive(Debug, Clone, Copy)]
pub struct HostCheck {
    /// Who refuses like this, as a developer would name it.
    pub server: &'static str,
    /// The status it refuses with.
    pub status: u16,
    /// Text every refusal of its carries: all of it, in any order.
    pub markers: &'static [&'static str],
    /// The change that lets every host ending in `{suffix}` in.
    pub remedy: &'static str,
}

pub const HOST_CHECKS: [HostCheck; 4] = [
    // Vite from 5.4.12 and 6.0.9, and so the frameworks built on it.
    HostCheck {
        server: "Vite",
        status: 403,
        markers: &["Blocked request", "allowedHosts"],
        remedy: "add '{suffix}' to Vite's `server.allowedHosts`, in vite.config or the `vite` \
                 section of the framework's own config",
    },
    // Rails 6 and later, in development.
    HostCheck {
        server: "Rails",
        status: 403,
        markers: &["Blocked host"],
        remedy: "add `config.hosts << \"{suffix}\"` to config/environments/development.rb",
    },
    // Django with `DEBUG` on, whose empty `ALLOWED_HOSTS` means localhost.
    HostCheck {
        server: "Django",
        status: 400,
        markers: &["DisallowedHost"],
        remedy: "add '{suffix}' to `ALLOWED_HOSTS` in the settings module",
    },
    HostCheck {
        server: "webpack-dev-server",
        status: 403,
        markers: &["Invalid Host header"],
        remedy: "add '{suffix}' to `devServer.allowedHosts` in the webpack config",
    },
];

/// The dev server whose refusal of a host this answer is, if it is one.
pub fn refused_by(status: u16, answer: &str) -> Option<&'static HostCheck> {
    HOST_CHECKS
        .iter()
        .find(|check| check.status == status && check.markers.iter().all(|m| answer.contains(m)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_refusal_is_known_by_what_its_server_really_answers() {
        let vite = "Blocked request. This host (\"abc.trycloudflare.com\") is not allowed.\n\
                    To allow this host, add \"abc.trycloudflare.com\" to `server.allowedHosts` \
                    in vite.config.js.";
        assert_eq!(refused_by(403, vite).map(|c| c.server), Some("Vite"));
        assert_eq!(
            refused_by(403, "<h1>Blocked hosts: abc.trycloudflare.com</h1>").map(|c| c.server),
            Some("Rails")
        );
        assert_eq!(
            refused_by(400, "DisallowedHost at /\nInvalid HTTP_HOST header").map(|c| c.server),
            Some("Django")
        );
        assert_eq!(
            refused_by(403, "Invalid Host header").map(|c| c.server),
            Some("webpack-dev-server")
        );
    }

    // An application's own page can say anything; only a refusal's
    // status and its words together are one.
    #[test]
    fn a_page_that_merely_mentions_a_refusal_is_not_one() {
        assert!(refused_by(200, "Blocked request … allowedHosts").is_none());
        assert!(refused_by(403, "Forbidden").is_none());
        assert!(
            refused_by(403, "Blocked request").is_none(),
            "Vite says both"
        );
    }

    #[test]
    fn every_remedy_names_the_suffix_it_lets_in() {
        for check in HOST_CHECKS {
            assert!(check.remedy.contains("{suffix}"), "{}", check.server);
        }
    }
}
