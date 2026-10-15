# Security policy

## Reporting a vulnerability

Report security issues privately to **security@hystax.com**, not in a public
issue. Include the version (`kompressor version`), what you ran, and what you
observed. We acknowledge within two working days and will tell you whether we
consider it a vulnerability, and when a fix is expected.

Please do not open a public issue, pull request or discussion for anything you
believe is exploitable until a fix has shipped.

## What is in scope

OptScale Kompressor handles agent traffic, so the interesting surfaces are the
ones that cross a trust boundary:

- **The proxy and the HTTP API.** Anything that lets one tenant read another
  tenant's fragments, or that forwards a request to an upstream other than the
  one its route names.
- **The fragment store.** Handles are content digests, not access tokens — they
  are predictable by design, and the tenant check is what separates callers. A
  path that serves a fragment without that check is a vulnerability.
- **`wrap`.** It edits an agent's own configuration file and restores it on
  exit. Anything that leaves credentials in a readable place, or that fails to
  restore a file it took ownership of, is in scope.
- **Fail-open behaviour.** The engine degrades to forwarding the original on
  error. A path that instead forwards something *else* — a different tenant's
  content, a truncated body presented as complete — is in scope.

## What is not in scope

- **Fragment storage is world-readable by default.** `refs.db` holds elided tool
  output verbatim at the default umask. On a shared machine, put `refs.path`
  somewhere with restricted permissions. This is documented behaviour, not a
  vulnerability.
- **No authentication by default.** The shipped configuration is a
  single-tenant local setup with auth off. Exposing it on a network without
  configuring `auth` is a deployment choice.
- Denial of service through deliberately pathological payloads, unless it
  escapes the engine's own guards (timeouts, size caps, panic catching).

## Supported versions

The most recent release receives security fixes. There are no long-term support
branches.
