# OptScale Kompressor on macOS, with Codex

A short walkthrough: install the package, point Codex at it, check it is
working. [`QUICKSTART.md`](QUICKSTART.md) §9 has the reference version of all of
this, including removal; [`CODEX.md`](CODEX.md) explains *why* the Codex side
needs the key it needs.

## 1. Get a package

Built on any machine with Docker — a Mac is not required:

```bash
make macos-pkg-docker
```

That leaves `target/macos/kompressor-<version>-universal2.pkg`, a universal
binary covering Apple Silicon and Intel. Copy it to the Mac.

## 2. Install it

```bash
sudo installer -pkg kompressor-*-universal2.pkg -target /
```

`-target /` names the destination **volume** — your startup disk — not a
directory to overwrite. The payload lands entirely under `/usr/local`.

Do not double-click it. The package is **unsigned**, so Gatekeeper refuses that;
the command above works, as does right-click → Open in Finder. Since the
installer runs a script as root, read it first if you want to:

```bash
pkgutil --expand kompressor-*-universal2.pkg /tmp/kpkg && cat /tmp/kpkg/Scripts/postinstall
```

The install places the binary in `/usr/local/bin`, writes
`~/.config/kompressor/config.toml` (never overwriting an existing one), and
loads a **LaunchAgent** that starts at every login.

## 3. Check it is running

```bash
curl -sf http://127.0.0.1:8817/healthz && echo OK
```

```bash
launchctl print gui/$(id -u)/com.kompressor.proxy | head -5
```

If the agent did not load — an install with no one logged in — finish it as
yourself, **not** with sudo:

```bash
/usr/local/share/kompressor/install-agent.sh
```

## 4. Point Codex at it

One line in `~/.codex/config.toml`:

```toml
openai_base_url = "http://127.0.0.1:8817/backend-api/codex"
```

That targets the `codex-desktop-chatgpt` mount, which forwards to
`chatgpt.com/backend-api/codex/…` with the prefix intact and no caller
classification. Your ChatGPT login is untouched: `forward_auth = true` means the
proxy passes your own credential through and never stores one.

Why this key and not an environment variable: under a ChatGPT plan, Codex routes
through its built-in `openai` provider whatever `model_provider` says, and only
the top-level `openai_base_url` catches that. `OPENAI_BASE_URL` is ignored.

> **Unverified for the Codex desktop app.** Everything in `CODEX.md` was
> measured against **codex-cli 0.155.1**. Whether the GUI reads the same config
> file, has its own base-URL setting, or ignores the key entirely is not known
> here. Step 5 is how you find out.

## 5. Confirm traffic is flowing

Start a short Codex session, then:

```bash
tail -f ~/Library/Logs/kompressor.log
```

Lines like `"message":"compressed","saved_tokens":12679` mean it is routing
through. Silence means Codex is not honouring the setting — check whether the
app has its own base-URL field in preferences.

Then the numbers:

```bash
kompressor stats summary
```

No flags needed, and this is where macOS is genuinely easier than the Linux
system service. The config lives at `~/.config/kompressor/config.toml`, which is
on the loader's search path, and the store is under your own home directory. The
Linux package installs its config to `/etc/kompressor/config.toml`, which is
**not** searched, so the same command there needs the path naming explicitly.

## Where the config is read from

The installer writes `~/.config/kompressor/config.toml`, but that is the last of
three places the loader looks. In order, first match wins:

| | |
|---|---|
| 1 | `$KOMPRESSOR_CONFIG` |
| 2 | `./kompressor.toml` — the working directory |
| 3 | `$XDG_CONFIG_HOME/kompressor/config.toml`, or `~/.config/kompressor/config.toml` when that variable is unset |

Two of these bite in practice. `$KOMPRESSOR_CONFIG` is used **even when the file
does not exist**, and the error names the path — deliberately, because a typo in
an explicit setting should be reported rather than silently fall through to a
different file and leave you reading the wrong numbers. And `./kompressor.toml`
means the directory you are standing in can change which config a command gets,
which is convenient for a project-local override and surprising otherwise: a
`kompressor stats summary` run from a checkout that happens to contain one is
reading a different store than the same command run from your home directory.

Note that `XDG_CONFIG_HOME` is honoured on macOS. It is not a conventional macOS
variable, but if something in your shell profile sets it, it takes precedence
over `~/.config` and the installed file is ignored.

Naming the file explicitly takes the search path out of the question — this
does not report which one *would* have been chosen, it removes the choice:

```bash
kompressor stats summary --config ~/.config/kompressor/config.toml
```

Whichever file is read, `KOMPRESSOR__*` environment variables are layered over
it, so a setting can differ from the file without the file being wrong —
`KOMPRESSOR__PROXY__WEBSOCKET_RELAY=false` is the usual example, since that one ships on.

The config is **not** under `~/Library/Application Support/kompressor/` — that
directory holds the state (the stats and refs databases), which is a separate
thing with a separate lifetime: the config survives an upgrade untouched, and
the store migrates on the first start of the new binary.

## Upgrading

Install the newer package the same way — `sudo installer -pkg … -target /`.
The identifier does not change between releases, so this replaces the component
rather than adding a second one. Your `~/.config/kompressor/config.toml` is kept
(it is only written when absent), the store under `~/Library/Application
Support/kompressor/` is left alone and migrates on the first start, and the
agent is reloaded so the new binary is running before the installer finishes.

The one thing that does **not** survive is the LaunchAgent plist: it is
regenerated from a template on every install, with your home directory
substituted in. Keep your changes in the config file instead.

## What to expect

**It starts at login, not at boot**, and does not run while you are logged out.
That is what a LaunchAgent is: the proxy serves the agents in your login session
and reads your config, which a root daemon has no business doing.

**A subscription session pays a WebSocket handshake.** `openai_base_url`
overrides the built-in provider's URL but cannot disable its WebSocket
preference, so expect five attempts before it falls back to HTTPS. Not a
misconfiguration.

| | |
|---|---|
| binary | `/usr/local/bin/kompressor` |
| config | `~/.config/kompressor/config.toml` (search order above) |
| state | `~/Library/Application Support/kompressor/` |
| logs | `~/Library/Logs/kompressor.log` |
| agent | `~/Library/LaunchAgents/com.kompressor.proxy.plist` |
