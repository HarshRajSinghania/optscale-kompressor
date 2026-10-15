//! Starting, sharing and stopping the proxy a wrap session runs behind.
//!
//! The awkward part is not starting one. It is that two `kompressor wrap`
//! sessions in two terminals should share a proxy, and neither of them knows
//! about the other. The rule that makes that work is reference counting through
//! the filesystem: each session drops a marker file naming itself, the last
//! session to remove its marker stops the proxy, and a marker whose owner has
//! died does not count. See [`super::paths::ProcessId`] for why "has died" is
//! answered with a start time rather than a bare pid.
//!
//! A proxy this process did **not** start is never stopped by it. Someone ran
//! `kompressor serve` deliberately; a wrap session exiting is not a reason to
//! take it away.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use kompressor_core::config::{Config, Upstream};

use super::paths::Paths;
use crate::error::CliError;

/// How long to wait for a freshly started proxy to answer `/healthz`.
///
/// Generous because the first start compiles nothing but does open a SQLite
/// database and, with `code-aware`, registers nine tree-sitter grammars. A
/// timeout that fires on a loaded machine would send the user hunting for a
/// configuration problem that is not there.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const READY_POLL: Duration = Duration::from_millis(200);

/// How the proxy behind this session came to exist, which decides who may stop
/// it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ownership {
    /// This session started it, so this session may stop it.
    Started,
    /// It was already listening. Left alone on exit.
    Adopted,
}

pub struct Proxy {
    pub port: u16,
    pub ownership: Ownership,
    pub log_path: PathBuf,
}

/// Whether something is already serving `/healthz` on `port`.
pub async fn is_serving(client: &reqwest::Client, port: u16) -> bool {
    client
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .timeout(Duration::from_millis(1500))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Whether a TCP port can be bound right now.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The first free port at or above `start`.
///
/// Only consulted when the preferred port is occupied by something that is not
/// a kompressor proxy — a wrap session must not fail because an unrelated
/// service happens to own 8817.
pub fn find_free_port(start: u16) -> Result<u16, CliError> {
    (start..start.saturating_add(64))
        .find(|port| port_is_free(*port))
        .ok_or_else(|| {
            CliError::Runtime(format!(
                "no free port in {start}..{}; pass --port to choose one",
                start.saturating_add(64)
            ))
        })
}

/// Write the configuration the supervised proxy will run under.
///
/// Built by layering the agent's upstreams onto the user's own configuration
/// rather than replacing it, so a wrap session honours the mode, the store
/// paths and the retention the user already configured. An upstream whose name
/// the user has already defined is left alone: an explicit mount is a decision,
/// and silently overriding it would route their traffic somewhere they did not
/// choose.
pub fn write_config(
    paths: &Paths,
    base: &Config,
    upstreams: Vec<Upstream>,
    port: u16,
) -> Result<PathBuf, CliError> {
    let mut config = base.clone();
    for upstream in upstreams {
        if config
            .proxy
            .upstreams
            .iter()
            .any(|existing| existing.name == upstream.name)
        {
            continue;
        }
        config.proxy.upstreams.push(upstream);
    }
    // `upstreams` is ordered longest-prefix-first by `agents::upstreams_for`,
    // but a user's own mounts come first and may be broader. Sorting by prefix
    // length keeps "first match wins" from depending on who wrote which entry.
    config
        .proxy
        .upstreams
        .sort_by_key(|u| std::cmp::Reverse(u.path_prefix.len()));

    let dir = paths.run_dir();
    super::paths::ensure_dir(&dir)
        .map_err(|e| CliError::runtime(&format!("could not create {}", dir.display()), e))?;
    let path = paths.proxy_config(port);
    let text = toml::to_string_pretty(&config)
        .map_err(|e| CliError::runtime("could not serialise the proxy configuration", e))?;
    std::fs::write(&path, text)
        .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e))?;
    Ok(path)
}

/// Start a proxy on `port` and wait until it answers.
///
/// Spawned into its own process group so that a Ctrl-C meant for the wrapped
/// agent cannot reach it. Without that, interrupting the agent would also kill a
/// proxy that other wrap sessions are still using.
pub async fn start(
    paths: &Paths,
    client: &reqwest::Client,
    port: u16,
    config_path: &std::path::Path,
) -> Result<Proxy, CliError> {
    let log_dir = paths.log_dir();
    super::paths::ensure_dir(&log_dir)
        .map_err(|e| CliError::runtime(&format!("could not create {}", log_dir.display()), e))?;
    let log_path = paths.proxy_log(port);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| CliError::runtime(&format!("could not open {}", log_path.display()), e))?;
    let log_err = log
        .try_clone()
        .map_err(|e| CliError::runtime("could not duplicate the log handle", e))?;

    let exe = std::env::current_exe()
        .map_err(|e| CliError::runtime("could not find this executable", e))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("serve")
        // The proxy is what the agent talks to; the API comes along because
        // `/healthz` lives on it, and a supervised process with no readiness
        // endpoint can only be waited for by guessing. It also gives the user a
        // working `kompressor retrieve` against the same fragment store while
        // the session is up. Both surfaces share one listener, so this costs a
        // router merge and nothing else.
        .arg("--proxy")
        .arg("--api")
        .arg("--config")
        .arg(config_path)
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|e| CliError::runtime("could not start the proxy", e))?;

    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    loop {
        if is_serving(client, port).await {
            return Ok(Proxy {
                port,
                ownership: Ownership::Started,
                log_path,
            });
        }
        // A proxy that exited has a reason in its log, and printing the tail
        // here is the difference between "it did not start" and knowing why.
        if let Ok(Some(status)) = child.try_wait() {
            let tail = log_tail(&log_path, 24);
            return Err(CliError::Runtime(format!(
                "the proxy exited with {status} before it began serving.\n\
                 Last lines of {}:\n{tail}",
                log_path.display()
            )));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            return Err(CliError::Runtime(format!(
                "the proxy did not answer /healthz on port {port} within {}s. \
                 Its log is {}",
                READY_TIMEOUT.as_secs(),
                log_path.display()
            )));
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

fn log_tail(path: &std::path::Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "  (the log could not be read)".to_string();
    };
    let kept: Vec<&str> = text.lines().rev().take(lines).collect();
    if kept.is_empty() {
        return "  (the log is empty)".to_string();
    }
    kept.into_iter()
        .rev()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Register this process as a client of the proxy on `port`.
pub fn register_client(paths: &Paths, port: u16) -> std::io::Result<()> {
    let dir = paths.clients(port);
    super::paths::ensure_dir(&dir)?;
    let me = super::paths::ProcessId::current();
    let body = match me.start_time {
        Some(start) => format!("{} {}\n", me.pid, start),
        None => format!("{}\n", me.pid),
    };
    std::fs::write(dir.join(me.pid.to_string()), body)
}

/// Drop this process's client marker.
pub fn unregister_client(paths: &Paths, port: u16) {
    let path = paths.clients(port).join(std::process::id().to_string());
    let _ = std::fs::remove_file(path);
}

/// How many *other* live sessions are using the proxy on `port`.
///
/// Sweeps dead markers as it goes. That is not tidiness: a marker left by a
/// crashed session would otherwise keep the proxy alive forever, and the sweep
/// is the only thing that ever removes it.
pub fn other_live_clients(paths: &Paths, port: u16) -> usize {
    let dir = paths.clients(port);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let me = std::process::id();
    let mut live = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(pid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let recorded = std::fs::read_to_string(&path).unwrap_or_default();
        let start_time = recorded
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u64>().ok());
        let holder = super::paths::ProcessId { pid, start_time };
        if holder.is_alive() && !(start_time.is_none() && super::paths::marker_is_stale(&path)) {
            live += 1;
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    live
}

/// Ask the proxy on `port` to shut down, and wait briefly for the port to free.
///
/// There is no admin endpoint, so this is a signal to the process group the
/// proxy was started in. Only ever called for a proxy this session started.
pub async fn stop(port: u16) {
    let Some(pid) = serving_pid(port) else {
        return;
    };
    // SIGTERM: `serve` shuts down gracefully on it and drains queued statistics,
    // which a SIGKILL would discard.
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    for _ in 0..50 {
        if port_is_free(port) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The pid listening on `port`, read from `/proc/net/tcp` via `ss` where
/// available. Returns `None` rather than guessing.
fn serving_pid(port: u16) -> Option<u32> {
    let output = std::process::Command::new("ss")
        .args(["-lptnH", &format!("sport = :{port}")])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let marker = text.find("pid=")?;
    text[marker + 4..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch layout per test. No environment is touched, so these run in
    /// parallel with everything else in the binary.
    fn scratch(tag: &str) -> (Paths, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "kompressor-wrap-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (Paths::at(&dir), dir)
    }

    #[test]
    fn a_registered_client_does_not_count_itself() {
        let (paths, dir) = scratch("selfcount");
        register_client(&paths, 9101).expect("marker written");
        assert_eq!(
            other_live_clients(&paths, 9101),
            0,
            "this process is the only client, and it is not an *other* client"
        );
        unregister_client(&paths, 9101);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dead_peers_marker_is_swept_rather_than_believed() {
        let (paths, dir) = scratch("sweep");
        let clients = paths.clients(9102);
        super::super::paths::ensure_dir(&clients).unwrap();
        // pid 0 never exists; the start time makes it unambiguously a corpse.
        std::fs::write(clients.join("0"), "0 12345\n").unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(
                other_live_clients(&paths, 9102),
                0,
                "a dead marker counts for nothing"
            );
            assert!(
                !clients.join("0").exists(),
                "and it is removed, or it would pin the proxy forever"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unregistering_is_idempotent() {
        let (paths, dir) = scratch("idempotent");
        register_client(&paths, 9103).unwrap();
        unregister_client(&paths, 9103);
        unregister_client(&paths, 9103); // must not panic on an already-removed marker
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_generated_config_keeps_the_users_own_upstream() {
        let (paths, dir) = scratch("config");
        let mut base = Config::default();
        base.proxy.upstreams.push(Upstream {
            name: "anthropic".into(),
            path_prefix: "/v1/messages".into(),
            base_url: "http://my-own-gateway.internal".into(),
            ..Default::default()
        });
        let path = write_config(
            &paths,
            &base,
            super::super::agents::upstreams_for(&[super::super::agents::Provider::Anthropic]),
            9104,
        )
        .expect("config written");
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            text.contains("my-own-gateway.internal"),
            "an explicit mount is a decision and must survive"
        );
        assert!(
            !text.contains("api.anthropic.com"),
            "and must not be shadowed by the built-in default"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_generated_config_adds_what_is_missing_longest_prefix_first() {
        let (paths, dir) = scratch("order");
        let path = write_config(
            &paths,
            &Config::default(),
            super::super::agents::upstreams_for(&[
                super::super::agents::Provider::Anthropic,
                super::super::agents::Provider::OpenAi,
            ]),
            9105,
        )
        .expect("config written");
        let text = std::fs::read_to_string(path).unwrap();
        let anthropic = text.find("/v1/messages").expect("anthropic mounted");
        let openai = text.find("api.openai.com").expect("openai mounted");
        assert!(
            anthropic < openai,
            "the narrower prefix has to be matched first"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_free_port_is_found_above_an_occupied_one() {
        let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        let found = find_free_port(port).expect("some port is free");
        assert_ne!(found, port, "the occupied port must not be offered");
        assert!(found > port);
    }
}
