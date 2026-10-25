# Security policy

## Supported versions

The `main` branch and the latest `v0.x` release receive security fixes.

## Reporting a vulnerability

**Do not open a public issue for a security problem.**

Report it privately through GitHub: open the repository's **Security** tab and
choose **Report a vulnerability** (private vulnerability reporting). If that
form is unavailable, open an issue that asks the maintainers for a private
channel without describing the details.

Please include:

- the version or commit, the platform, and how CodeRex was built;
- a minimal reproduction (prompt, configuration, plugin/mod if relevant);
- the impact you believe it has.

We aim to acknowledge a report within three business days.

## Scope

CodeRex runs model-proposed shell commands, reads and writes files, and loads
user-installed plugins and mods. A report is in scope when it shows a way to
cross one of the boundaries the project claims:

- **Sandbox and permissions** — escaping Seatbelt, bubblewrap/Landlock or an
  AppContainer, or running a command outside the configured permission mode.
- **Extension limits** — a hook, plugin, WASM module or in-process mod escaping
  its documented limits (wall clock, heap, process tree, worker isolation).
- **Credentials** — disclosing secrets through logs, transcripts, child
  environments or telemetry that the configuration did not opt into.
- **Prompt injection** — turning untrusted content (web pages, MCP servers,
  repository files) into actions the person did not approve.

Out of scope:

- the quality or safety of model output by itself;
- vulnerabilities in third-party MCP servers or model providers;
- issues that require an already-compromised account or machine;
- the known boundary that an in-process mod runs with the user's privileges
  (documented in `docs/mods.md`); mods must be code the person chose to install.

## Telemetry

Telemetry is opt-in. With the default settings nothing is sent; see
[`docs/telemetry.md`](docs/telemetry.md) for what is collected when it is
enabled and how to disable it.
