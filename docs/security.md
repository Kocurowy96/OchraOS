# Security

Security is priority number one.

## Basics

- **Accounts:** the first user is an administrator. Login via greetd with an iced front-end.
- **Privilege elevation:** `sudo-rs` in the terminal, polkit in the GUI. The user picks the
  confirmation style: password, UAC-style click, or a hardware key touch. The dialog is drawn
  by the compositor, so no app can click or fake it. No "no confirmation" mode by default.
- **Disk encryption:** enabled by default in the installer, can be unticked (from stage 7).
- **Updates:** A/B images signed with Ed25519 and verified before install.
- **Kernel:** SMEP/SMAP, KASLR, W^X, `unsafe` confined to one crate.
- **W^X for processes:** by default no user memory is writable and executable at once (ELF
  segments give `ENOEXEC`, `mmap`/`mprotect` give `EACCES`). JIT compilers get an exception
  per application through Ochra Guard (see below).
- **AVX state:** CR4.OSXSAVE stays off until the kernel saves the extended state with
  `xsave` on task switches; until then programs see no AVX (CPUID reports OSXSAVE clear), so
  they cannot leave AVX registers to leak into another process.
- **Meltdown:** user page tables map the kernel half (supervisor-only) for now. Before
  OchraOS runs on a CPU affected by Meltdown (e.g. Haswell, as in the ThinkPad T440p), the
  kernel needs KPTI (separate user page tables without the kernel), switched off when
  `IA32_ARCH_CAPABILITIES` reports `RDCL_NO`. Not implemented yet.
- **Firewall:** at first no service listens externally; nftables compatibility later.

## Ochra Guard

Data protection modelled on macOS TCC, but permissions belong to a **process**, not an
application — consent for one terminal command does not leak to the next. Enforced in the
kernel, so it applies to root too.

| Level | Examples | Rule |
| --- | --- | --- |
| Free | workspaces (e.g. `~/dev`), Downloads | no prompts |
| Private | Documents, Pictures | ask once per program, remembered |
| Secrets | browser cookies and passwords, SSH and GPG keys, password managers | owner app only; others ask on every access, consent expires after minutes, never "always" for generic tools |
| System | `/usr`, system configuration | signed system services only |

- Consent scopes: "this command only" (default), "until this terminal tab closes", "always for
  this program". The prompt shows the full command line.
- Interpreters (python, bash, node): consent is bound to the script (path + hash).
- **Secrets + network:** a process that read secrets (and its children) must ask before
  sending anything over the network.
- **Wine:** no `Z:` drive mapping of `/` by default.
- **JIT (writable and executable memory):** an exception to W^X granted per application with
  the user's consent (like macOS's `allow-jit` entitlement). Ready-made profiles for
  Wine/Proton (Windows programs often allocate `PAGE_EXECUTE_READWRITE`) and the JVM.
  Implemented together with Wine (stage 7).
- Audit log in Settings with revocation; a weekly privacy summary.
- Camera, microphone, screen capture, location: consent + indicator in the panel.
- Clipboard: readable only by the focused application.
- Ransomware: a process mass-rewriting private files is paused with a prompt.
- Later: per-app outbound firewall (Little Snitch style).
- Kernel hooks designed during stages 2–4; full Ochra Guard with the GUI at stages 6–7.

### USB protection (BadUSB)

1. A new HID device (keyboard) is not activated until Ochra Guard allows it; its keystrokes do
   not reach the system.
2. Approval of a new keyboard = typing a random code on that keyboard (like a Bluetooth PIN),
   not just clicking "Allow". Typos can be retried; input that does not match the code
   disconnects the device and raises an alert.
3. Exception: if no other working input device exists (e.g. the first keyboard of a desktop
   PC), the keyboard works immediately under strict typing-rhythm monitoring.
4. Injection detection: a longer run of keystrokes a few milliseconds apart with near-zero
   variance blocks the device. Single short gaps (key rollover) are not enough. Held
   keystrokes are shown in the alert ("this device tried to type: ...").
5. Lock screen: new unknown HID devices are blocked; known ones may work (docking stations),
   with injection detection and a password attempt limit.
6. Disk encryption: the boot-time password is typed before Ochra Guard runs, so the
   bootloader and early boot accept keyboards without these rules.
7. Legitimate fast "typists" — barcode scanners, YubiKey OTP, gaming keyboard macros — are
   marked once by the user after passing the code test.
8. Composite devices (storage + keyboard) raise suspicion but are not proof of an attack.
9. Device IDs (VID, PID, serial) are self-reported and spoofable: they only spare repeated
   prompts; injection detection always stays on. Real trust comes only from cryptographic
   signatures (Kocur Link).

## Hardware keys (FIDO2: YubiKey, Nitrokey, SoloKeys)

- Login as a second factor (password + key) or passwordless (key + PIN).
- Privilege confirmation by touching the key.
- Disk unlock at boot, always with a password or recovery key as fallback.
- Optional: removing the key locks the screen.
- From stage 7.

## Supply chain

Lesson from the xz backdoor (2024): open code allows audits but does not guarantee them, and
attacks can come through trust and pressure on a maintainer.

- Build only from Git sources, never from release tarballs. CI flags unexplained binary files.
- Reproducible builds: the same source gives the same image, bit for bit.
- Commit rights are granted slowly. Changes to CI, build scripts and Ochra Guard always need
  review by two people.
- Pressure like "add me as maintainer, you can't keep up" is a warning sign.
- Arch packages are an external dependency: follow Arch security advisories, be able to pin a
  package version in the image quickly.
- Vulnerability reports: `SECURITY.md`, fast response, public thanks in the Hall of Fame.

## Signing keys

- Images are signed with plain Ed25519 (minisign style), not PGP.
- Hierarchy:
  1. **Root key** — generated offline, encrypted backups kept offline, used rarely. OchraOS
     trusts only this key.
  2. **Release key** — on a YubiKey, certified by the root key, signs Stable releases.
  3. **Nightly/Beta key** — in CI secrets; a CI compromise can only forge test builds.
- Lost YubiKey: revoke and certify a new release key with the root key.
- Signing keys never live on any server (cloud or homelab).
- Commits signed with an SSH key; GitHub login with a hardware key; signed commits required
  on `main`.
