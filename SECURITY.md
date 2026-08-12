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

Also in scope: flaws in the release pipeline or the install scripts.

## Known limitations (by design, not vulnerabilities)

These are documented behaviours. Please don't file them as vulnerabilities —
but do file an issue if you think the trade-off is wrong.

- **webseek fetches whatever URL it is given.** There is no allow-list and no
  filtering of private address ranges, so `webseek fetch
  http://169.254.169.254/…` will reach a cloud metadata endpoint just as
  `curl` would. If you point webseek at URLs from an untrusted source inside a
  privileged network, apply egress controls at the network layer.
- **robots.txt is advisory and opt-in.** With `--respect-robots`, only the `*`
  user-agent group is consulted, and an unreachable robots.txt is treated as
  "allowed".
- **Checksums are not signatures.** `SHA256SUMS.txt` is published alongside the
  archives from the same release, so it protects against a corrupted download,
  not against a compromised release. Verifying it is worthwhile; treat it as an
  integrity check rather than an authenticity guarantee.
- **macOS binaries are unsigned and unnotarised.**
