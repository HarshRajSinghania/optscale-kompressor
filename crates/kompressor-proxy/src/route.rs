//! Which upstream a path belongs to, and what URL the request is forwarded to.
//!
//! Routing is by the longest matching path prefix. A catch-all mount therefore
//! cannot hide a more specific provider just because it appears first.

use kompressor_core::config::{ProxyConfig, SubscriptionUpstream, Upstream};

/// The upstream a request was routed to and the URL it will be sent to.
#[derive(Clone, Debug)]
pub struct Route<'a> {
    /// The matched upstream.
    pub upstream: &'a Upstream,
    /// The absolute upstream URL, query string included.
    pub url: String,
    /// The path as the upstream will see it, which is what decides the API
    /// family: a mount prefix is ours, not the provider's.
    pub path: String,
}

impl Route<'_> {
    /// The origin the request is actually sent to, for the record this run
    /// leaves behind.
    ///
    /// Not `upstream.base_url`: that is the mount as configured, and under a
    /// subscription the same mount sends to a different host. Recording the
    /// configured value made every row of a live ChatGPT-plan session read
    /// `https://api.openai.com` for traffic that went to `chatgpt.com`.
    pub fn origin(&self) -> &str {
        let after_scheme = match self.url.find("://") {
            Some(at) => at + 3,
            None => 0,
        };
        match self.url[after_scheme..].find('/') {
            Some(slash) => &self.url[..after_scheme + slash],
            None => &self.url,
        }
    }
}

/// Find the upstream for a path.
///
/// An empty `path_prefix` matches everything, which is how a single-provider
/// deployment is configured. `default_upstream` is consulted only when no
/// prefix matched, so a catch-all cannot shadow a specific mount.
pub fn match_upstream<'a>(config: &'a ProxyConfig, path: &str) -> Option<&'a Upstream> {
    if let Some(upstream) = config
        .upstreams
        .iter()
        .filter(|upstream| {
            let prefix = mount_prefix(upstream);
            prefix.is_empty()
                || path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with('/'))
        })
        .enumerate()
        // Ranked on the prefix that actually matched, not on the configured
        // spelling: `/v1` and `/v1/` name the same mount point, and letting a
        // trailing slash outrank its absence would decide between two mounts on
        // a character the match itself has already declared insignificant.
        .max_by_key(|(index, upstream)| (mount_prefix(upstream).len(), std::cmp::Reverse(*index)))
        .map(|(_, upstream)| upstream)
    {
        return Some(upstream);
    }
    if config.default_upstream.is_empty() {
        return None;
    }
    config
        .upstreams
        .iter()
        .find(|upstream| upstream.name == config.default_upstream)
}

/// Build the route for a matched upstream.
///
/// `subscription` selects the upstream's [`SubscriptionUpstream`] override when
/// it has one: the same body belongs to a different host and a different path
/// depending on how the caller pays, and only the credential says which.
pub fn route_for<'a>(
    upstream: &'a Upstream,
    path: &str,
    query: Option<&str>,
    subscription: bool,
) -> Route<'a> {
    let over = upstream.subscription.as_ref().filter(|_| subscription);
    let forwarded = match over {
        Some(over) => subscription_path(upstream, over, path),
        None => forwarded_path(upstream, path),
    };
    let base = over
        .map(|o| o.base_url.as_str())
        .unwrap_or(&upstream.base_url)
        .trim_end_matches('/');
    let mut url = format!("{base}{forwarded}");
    if let Some(query) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(query);
    }
    Route {
        upstream,
        url,
        path: forwarded,
    }
}

/// The path a subscription caller's request takes upstream.
///
/// The mount prefix is replaced rather than stripped: what follows it is the
/// endpoint the client asked for, and the provider keeps that part intact under
/// both credentials — it is only the prefix in front of it that moves.
fn subscription_path(upstream: &Upstream, over: &SubscriptionUpstream, path: &str) -> String {
    if over.rewrite_prefix.is_empty() {
        return forwarded_path(upstream, path);
    }
    let rest = path.strip_prefix(mount_prefix(upstream)).unwrap_or(path);
    let prefix = over.rewrite_prefix.trim_end_matches('/');
    if rest.starts_with('/') {
        format!("{prefix}{rest}")
    } else {
        format!("{prefix}/{rest}")
    }
}

/// The mount point a prefix names, with a trailing slash removed.
///
/// One spelling, used by the match, the ranking and the strip alike. They used
/// to disagree: the match trimmed the slash and the strip did not, so a mount
/// written `/ollama/` matched the bare path `/ollama` and then failed to strip
/// anything from it — forwarding the mount prefix to a provider that has never
/// heard of it.
fn mount_prefix(upstream: &Upstream) -> &str {
    upstream.path_prefix.trim_end_matches('/')
}

/// The path the upstream sees.
///
/// Stripping a prefix must not produce a relative path: `/ollama/` with
/// `strip_prefix` leaves nothing at all, and a URL built from that would point
/// at the upstream's root rather than at the endpoint the client asked for.
fn forwarded_path(upstream: &Upstream, path: &str) -> String {
    let prefix = mount_prefix(upstream);
    if !upstream.strip_prefix || prefix.is_empty() {
        return path.to_string();
    }
    let rest = path.strip_prefix(prefix).unwrap_or(path);
    if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(name: &str, prefix: &str, strip: bool) -> Upstream {
        Upstream {
            name: name.to_string(),
            path_prefix: prefix.to_string(),
            strip_prefix: strip,
            base_url: format!("https://{name}.example"),
            ..Default::default()
        }
    }

    fn config(upstreams: Vec<Upstream>, default: &str) -> ProxyConfig {
        ProxyConfig {
            upstreams,
            default_upstream: default.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_first_matching_prefix_wins() {
        let config = config(
            vec![
                upstream("anthropic", "/v1/messages", false),
                upstream("openai", "", false),
            ],
            "",
        );
        assert_eq!(
            match_upstream(&config, "/v1/messages").map(|u| u.name.as_str()),
            Some("anthropic")
        );
        assert_eq!(
            match_upstream(&config, "/v1/chat/completions").map(|u| u.name.as_str()),
            Some("openai"),
            "an empty prefix is the catch-all"
        );
    }

    #[test]
    fn a_catch_all_cannot_shadow_a_specific_mount() {
        let config = config(
            vec![
                upstream("default", "", false),
                upstream("anthropic", "/v1/messages", false),
            ],
            "",
        );
        assert_eq!(
            match_upstream(&config, "/v1/messages").map(|u| u.name.as_str()),
            Some("anthropic")
        );
        assert_eq!(
            match_upstream(&config, "/v1/messages-extra").map(|u| u.name.as_str()),
            Some("default"),
            "a prefix must end at a path segment boundary"
        );
    }

    #[test]
    fn a_mount_written_with_a_trailing_slash_strips_it_the_same_way() {
        let mut mount = upstream("ollama", "/ollama/", true);
        mount.base_url = "https://ollama.example".into();
        // The bare mount point maps to the upstream root, not to a path the
        // provider has never heard of.
        assert_eq!(
            route_for(&mount, "/ollama", None, false).url,
            "https://ollama.example/"
        );
        assert_eq!(
            route_for(&mount, "/ollama/v1/chat", None, false).url,
            "https://ollama.example/v1/chat"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_decide_between_two_mounts() {
        let config = config(
            vec![
                upstream("plain", "/v1", false),
                upstream("slashed", "/v1/", true),
            ],
            "",
        );
        assert_eq!(
            match_upstream(&config, "/v1/responses").map(|u| u.name.as_str()),
            Some("plain"),
            "same mount point, so the earlier one wins as it always did"
        );
    }

    #[test]
    fn an_unmatched_path_falls_back_only_to_a_named_default() {
        let specific = config(vec![upstream("anthropic", "/v1/messages", false)], "");
        assert!(match_upstream(&specific, "/v1/chat/completions").is_none());

        let defaulted = config(
            vec![upstream("anthropic", "/v1/messages", false)],
            "anthropic",
        );
        assert_eq!(
            match_upstream(&defaulted, "/v1/chat/completions").map(|u| u.name.as_str()),
            Some("anthropic")
        );
    }

    #[test]
    fn a_stripped_mount_prefix_leaves_an_absolute_upstream_path() {
        let ollama = upstream("ollama", "/ollama", true);
        let route = route_for(
            &ollama,
            "/ollama/v1/chat/completions",
            Some("stream=1"),
            false,
        );
        assert_eq!(route.path, "/v1/chat/completions");
        assert_eq!(
            route.url,
            "https://ollama.example/v1/chat/completions?stream=1"
        );

        // stripping the whole path must not leave a relative URL pointing at
        // the upstream's root
        let bare = route_for(&ollama, "/ollama", None, false);
        assert_eq!(bare.path, "/");
    }

    #[test]
    fn a_mount_prefix_that_is_kept_reaches_the_upstream_as_it_arrived() {
        let openai = upstream("openai", "/v1", false);
        let route = route_for(&openai, "/v1/chat/completions", None, false);
        assert_eq!(route.path, "/v1/chat/completions");
        assert_eq!(route.url, "https://openai.example/v1/chat/completions");
    }

    /// The mount a `kompressor wrap codex` session installs.
    fn codex_mount() -> Upstream {
        Upstream {
            name: "openai".into(),
            path_prefix: "/v1".into(),
            base_url: "https://api.openai.com".into(),
            api_family: "openai".into(),
            subscription: Some(SubscriptionUpstream {
                base_url: "https://chatgpt.com".into(),
                rewrite_prefix: "/backend-api/codex".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_origin_is_the_host_actually_used_not_the_mount() {
        let mount = codex_mount();
        assert_eq!(
            route_for(&mount, "/v1/responses", None, true).origin(),
            "https://chatgpt.com"
        );
        assert_eq!(
            route_for(&mount, "/v1/responses", None, false).origin(),
            "https://api.openai.com"
        );
        // A query string is not part of the origin.
        assert_eq!(
            route_for(&mount, "/v1/responses", Some("stream=1"), false).origin(),
            "https://api.openai.com"
        );
    }

    #[test]
    fn a_subscription_caller_is_routed_to_the_subscription_host_and_path() {
        let mount = codex_mount();
        let route = route_for(&mount, "/v1/responses", None, true);
        assert_eq!(route.url, "https://chatgpt.com/backend-api/codex/responses");
        // The family is read off the forwarded path, so the rewrite has to
        // leave something the compressor still recognises.
        assert_eq!(route.path, "/backend-api/codex/responses");
        assert_eq!(
            kompressor_wire::detect_family(&route.path),
            Some(kompressor_wire::ApiFamily::OpenAiResponses)
        );
    }

    #[test]
    fn the_same_request_under_an_api_key_is_routed_to_the_ordinary_host() {
        let mount = codex_mount();
        let route = route_for(&mount, "/v1/responses", Some("x=1"), false);
        assert_eq!(route.url, "https://api.openai.com/v1/responses?x=1");
        assert_eq!(route.path, "/v1/responses");
    }

    #[test]
    fn a_mount_without_a_subscription_override_ignores_the_flag() {
        let plain = upstream("openai", "/v1", false);
        let payg = route_for(&plain, "/v1/responses", None, false);
        let subscribed = route_for(&plain, "/v1/responses", None, true);
        assert_eq!(payg.url, subscribed.url);
    }

    #[test]
    fn an_override_that_only_moves_the_host_leaves_the_path_alone() {
        let mut mount = codex_mount();
        mount.subscription = Some(SubscriptionUpstream {
            base_url: "https://chatgpt.com".into(),
            rewrite_prefix: String::new(),
        });
        let route = route_for(&mount, "/v1/responses", None, true);
        assert_eq!(route.url, "https://chatgpt.com/v1/responses");
    }
}
