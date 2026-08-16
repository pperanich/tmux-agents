//! Where a row came from: the tmux server it was observed on and the machine that observed it.
//!
//! A pane id is unique per tmux server and nothing more, so `%5` from a laptop and `%5` from a build
//! box collide the moment two `tma ls --json` outputs are merged. The `server`/`host` keys on every
//! emitted row are what make the merged set addressable. Both are resolved ONCE per invocation
//! ([`Origin::resolve`]) and threaded into the serializer: `server` costs one tmux call and `host`
//! one `uname`, neither of which belongs on a per-row path.

use tma_tmux::tmux::Tmux;

/// The provenance pair stamped on every JSON agent row.
#[derive(Clone, Debug, Default)]
pub struct Origin {
    /// The tmux server's `#{socket_path}` — the absolute socket the server is listening on, which is
    /// what the daemon's own per-server keying hashes ([`crate::ipc::socket_key`]). The path itself
    /// rather than that hash: it is equally stable, and an operator reading a merged log can tell
    /// `/private/tmp/tmux-501/default` from `/tmp/tmate-501/…` at a glance. Empty when the server is
    /// gone (nothing was observed on it either, so no row carries the empty value in practice).
    pub server: String,
    /// This machine's hostname (`uname` nodename). Empty only if the system reports none.
    pub host: String,
}

impl Origin {
    /// Resolve both once, for one invocation's worth of rows.
    pub fn resolve(tmux: &Tmux) -> Origin {
        Origin {
            server: crate::ipc::resolve_socket_path(tmux).unwrap_or_default(),
            host: hostname(),
        }
    }
}

/// This machine's hostname, via `uname(2)` through rustix (the workspace's safe syscall layer; no
/// libc). std exposes no hostname API, and `$HOSTNAME` is a shell variable that is often unexported.
pub fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hostname resolves to a non-empty label with no interior NUL or newline (it lands in a
    /// JSON string beside a pane id, and a newline there would break a line-oriented consumer).
    #[test]
    fn hostname_is_a_plain_non_empty_label() {
        let host = hostname();
        assert!(!host.is_empty(), "uname reported no nodename");
        assert!(!host.contains('\n') && !host.contains('\0'), "{host:?}");
        assert_eq!(host, hostname(), "stable across calls");
    }
}
