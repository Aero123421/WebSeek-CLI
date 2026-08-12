# Security Policy

## Supported versions

webseek is pre-1.0. Only the latest release receives fixes.

| Version | Supported |
|---|---|
| latest release | ✅ |
| anything older | ❌ |

## Reporting a vulnerability

Please **do not open a public issue** for a security problem.

Use GitHub's private reporting:
[Report a vulnerability](https://github.com/Aero123421/WebSeek-CLI/security/advisories/new).

Include the version (`webseek --version`), your platform, and the smallest
reproduction you can manage — ideally a URL or a saved response body that
triggers it.

Expect an acknowledgement within a week. There is no bounty programme.

## Threat model

webseek is a command-line client that fetches attacker-influenced content: an
agent typically feeds it URLs that came out of a search engine, so **response
bodies are untrusted input**. Bugs in that direction are in scope:

- Memory exhaustion, unbounded reads, or hangs triggered by a crafted page
  (webseek caps body size, output characters, output lines and element nesting
  depth precisely because of this).
- Panics reachable from parsing a response.
- Path traversal or unexpected writes via `images --download` filenames.
- Anything that causes webseek to send credentials, local file contents, or
  environment data to a remote host.
- Egress-policy bypasses that reach loopback, private, link-local or reserved
  destinations without the explicit `--allow-private` opt-out.
- Terminal-control injection through pretty output, and unsafe overwrite or
  symlink-following behavior in image downloads.

Also in scope: flaws in the release pipeline or the install scripts.

## Known limitations (by design, not vulnerabilities)

These are documented behaviours. Please don't file them as vulnerabilities —
but do file an issue if you think the trade-off is wrong.

- **Private-network access is an explicit opt-out.** By default webseek allows
  only HTTP(S), rejects URL credentials, and blocks loopback, private,
  link-local, multicast and reserved addresses on initial URLs, DNS answers
  and redirects. `--allow-private` / `allow_private_network = true` disables
  the address-range restriction for deliberate internal use.
- **Proxies move DNS enforcement outside the process.** A configured HTTP(S)
  proxy resolves the destination itself, so webseek cannot inspect that final
  IP. A warning is emitted when strict egress policy and a proxy environment
  variable are both active. Use `--no-proxy` / `allow_proxy = false` when the
  strongest local egress guarantee is required, and enforce policy at the
  proxy/network layer otherwise.
- **Opening a URL is not fetching it.** `--open` blocks non-HTTP schemes by
  default but permits private HTTP(S) destinations because the user's browser,
  not webseek, performs that request. Use `--allow-external-schemes` only when
  deliberately launching schemes such as `mailto:`.
- **robots.txt is advisory and opt-in.** With `--respect-robots`, only the `*`
  user-agent group is consulted, and an unreachable robots.txt is treated as
  "allowed".
- **Checksums are not signatures.** `SHA256SUMS.txt` is published alongside the
  archives from the same release, so it protects against a corrupted download,
  not against a compromised release. Verifying it is worthwhile; treat it as an
  integrity check rather than an authenticity guarantee.
- **macOS binaries are unsigned and unnotarised.**
