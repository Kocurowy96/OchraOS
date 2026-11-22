# Security policy

Security is OchraOS's top priority. Thank you for helping keep it safe.

## Reporting a vulnerability

**Do not open a public issue for security problems.**

Report privately through GitHub: the repository's **Security** tab → **Report a
vulnerability**. Include:

- the OchraOS version (`/etc/os-release`) and channel (Stable, Beta, Nightly),
- steps to reproduce, and what an attacker could achieve,
- a proof of concept if you have one.

## What to expect

- An acknowledgement within 7 days.
- A fix prepared privately and released as soon as possible, with a security advisory.
- Public credit in the release notes and in the in-system Hall of Fame, unless you prefer to
  stay anonymous.

## Supported versions

Only the latest Stable release and the current Beta/Nightly receive security fixes.

## Scope

In scope: the kernel, Ochra Guard, the updater and its signatures, the installer, and all
other OchraOS components in this repository. Vulnerabilities in third-party packages (Arch,
Flatpak apps, Wine) should be reported upstream; tell us too if they affect OchraOS
specifically.
