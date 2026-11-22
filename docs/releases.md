# Releases

## Channels

| Channel | What it is | Cadence |
| --- | --- | --- |
| Stable (default) | rolling, but as tested snapshots: Arch packages + OchraOS components frozen into an image and verified | every 2–4 weeks |
| Beta | candidate for the next Stable | weekly |
| Nightly | automatic CI build, may break | daily |

No LTS or LTSC for now: Arch does not keep old package versions, so LTS would mean an own
frozen repository and years of security backports. Reconsider if a community forms.

## Versioning

`year.quarter.number`, SemVer-compatible, so versions sort themselves and Beta/Nightly are
pre-releases of the next Stable:

- Stable: `2027.1.3`, displayed as "OchraOS Hematyt (Stable 2027.1.3)"
- Beta: `2027.2.0-beta.4+a1b2c3d`
- Nightly: `2027.2.0-nightly.20270415.1830+a1b2c3d`

The part after `+` is the short Git commit hash. The build machine goes into build metadata
(`/etc/os-release`), not into the version, to keep builds reproducible.

Kernel and components have their own SemVer versions (`0.x` until the first Stable).

## Codenames

One codename per quarter: first iron minerals (Hematyt, Magnetyt, Limonit, Goethyt, Syderyt…),
then elements of the periodic table. The mascot names (Bursztyn, Selenit, Tanzanit) are never
used as codenames.

## Easter egg wheel

At each new codename the release script spins a wheel (seeded from the version number, so the
draw is verifiable): 1 in 4 chance of a new easter egg. If the release falls on a holiday, the
easter egg refers to it. Easter eggs never go into Ochra Guard or the installer.

## Transparency

Tests and benchmarks (e.g. games on the RTX 4060) are public and reproducible.
