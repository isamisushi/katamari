# Language servers

katamari spawns servers lazily, the first time a file of that language is
opened, and looks for each one in this order: config override →
project-local convention (`node_modules/.bin`, `.venv/bin`) → `PATH` →
`rustup which`/`mise which` → katamari's own managed install (below). If
none of those find anything, katamari **auto-installs it** — downloading
rust-analyzer's prebuilt binary from GitHub releases, or running `npm
install`/`go install` into a private prefix — with progress shown in the
status bar, no confirmation prompt, the same "it just works" experience
VSCode/Zed give you.

| Language | Server | Auto-install strategy |
| --- | --- | --- |
| Rust | `rust-analyzer` | prebuilt binary from GitHub releases |
| TypeScript/JavaScript | `typescript-language-server` | `npm install` (bootstraps a private Node.js runtime first if no `npm` can be found anywhere) |
| Python | `pyright-langserver` | `npm install` (same Node.js bootstrap fallback) |
| Go | `gopls` | `go install`, requires an existing go toolchain — katamari won't install Go itself |
| Kotlin | `kotlin-lsp` (JetBrains) | prebuilt archive from JetBrains' CDN, bundling its own JVM — no external Java needed |
| Java | `jdtls` (Eclipse JDT LS) | prebuilt tarball from download.eclipse.org — needs an external JDK 21+ (katamari won't install a JVM) |

kotlin-lsp is JetBrains' own server (the `fwcd/kotlin-language-server`
community project is unmaintained as of this writing) and is still alpha
quality: on a project with no Gradle wrapper yet cached, its first hover,
go-to-definition, or diagnostics pull can take tens of seconds while it
resolves the classpath and indexes the project in the background —
katamari's `ktmr lsp-check` retries handle this the same way they do
rust-analyzer's own cold-start indexing. kotlin-lsp only implements
LSP 3.17's pull model (`textDocument/diagnostic`), never the unsolicited
`publishDiagnostics` push notifications every other server here sends;
katamari's gutter/`]d`/`[d`/`--diagnostics` flow now pulls on Kotlin's
behalf after every open/change and re-publishes the answer through the same
path a push would use, so error/warning highlighting works for Kotlin files
too — the only user-visible difference from a push server is that the very
first diagnostics for a freshly-opened file can lag behind hover/go-to
readiness while indexing finishes, since an early pull during that window
can legitimately come back empty. One more side effect worth knowing about:
importing a Gradle project spawns a Gradle daemon, which — by Gradle's own
design — keeps running after kotlin-lsp itself exits (including after a
`ktmr doctor` probe). That's normal daemon reuse, not a leak of katamari's;
`gradle --stop` (or killing the `GradleDaemon` process) reclaims it.

jdtls needs a JDK 21+ on the machine — `JAVA_HOME` is honored first, then
`PATH`, then `mise which java` — katamari installs the server itself but
never a JVM to run it on; `ktmr lsp doctor` prints a `jdk:` note under the
Java row naming the JDK it found and its version, saying so if the one it
found is too old, or reporting `not found` if there's no JDK anywhere.
First-open indexing can take a while on a large Maven/Gradle repo, during
which hover/go-to-definition may time out until the import finishes. Its
per-workspace index lives under
`$XDG_STATE_HOME/katamari/jdtls-workspaces/` (`~/.local/state/katamari/…`
if unset) — separate from the managed-server install below, and likewise
safe to delete to force a reindex.

Managed installs live under `~/.local/share/katamari/servers/`
(`$XDG_DATA_HOME/katamari/servers/` if set), one version-stamped
subdirectory per server — never touched by anything but katamari, and safe
to delete entirely (everything reinstalls on demand).

`[lsp.servers.<id>]` in config (see [Configuration](./configuration.md))
overrides any of these with an explicit command, taking priority over
every lookup including auto-install.
To disable auto-install and just get the old "here's the install command"
status-bar hint instead:

```toml
[lsp]
auto_install = false
```

## Custom language servers

`[lsp.servers.<id>]` isn't limited to overriding one of the six built-in
languages above — any `<id>` of your own choosing defines a server for a
filetype katamari has no built-in support for, as long as it claims at
least one file extension:

```toml
[lsp.servers.ruby]
command = "solargraph"
args = ["stdio"]
extensions = ["rb"]
root_markers = ["Gemfile"]
```

`extensions` is what makes `ruby` claim `.rb` files at all — a custom id
with no `extensions` is just an inert config entry (indistinguishable from
a plain built-in override with none of the new fields set). A leading `.`
is accepted and stripped (`extensions = [".rb"]` and `["rb"]` are
equivalent), and each entry is trimmed of surrounding whitespace. A custom
claim on an extension one of the six built-in languages already owns is
dropped, the built-in always winning; if two custom ids claim the same
extension, whichever id sorts first alphabetically wins instead; and
`extensions` set on an id that's itself one of the six built-in language
names (e.g. `[lsp.servers.rust]`) is ignored outright, since `<id>` is what
decides override-vs-custom — all three cases warn once to stderr, and
`ktmr lsp doctor`'s custom-server table flags an affected entry with a note
saying why its extensions don't route anywhere. `root_markers` finds this
id's workspace root the same way a built-in language's nearest-marker tier
does — the closest ancestor
directory containing one of the listed files, falling back to the
repository root if it's empty or nothing matches; there's no built-in-style
"workspace of workspaces" tier for a custom id; that logic is
adapter-specific knowledge (a Cargo `[workspace]` table, a `go.work`, a
Gradle `settings.gradle`) this module has no way to generalize to a server
it's never heard of. `language_id` overrides the LSP `languageId`
announced in `didOpen`, for the rare server that expects something other
than the `<id>` key itself.

A custom server is never auto-installed — `command` must already be
reachable on its own, and `ktmr lsp doctor` reports it (in a second table
beneath the built-in one) as found or not found the same way it does for a
built-in language; `ktmr lsp install` doesn't support a custom id.

`initialization_options` (TOML, converted to JSON at resolve time) works on
a built-in override too, not just a custom server — e.g. sending
rust-analyzer settings through `[lsp.servers.rust]`.

`ktmr lsp` manages installs directly, without waiting for a server to be
needed:

```
ktmr lsp doctor              # where each language's server resolves from today (no installs triggered) — see the Health check chapter for the fuller report
ktmr lsp install <language>  # force an install into katamari's managed prefix (rust/typescript/python/go/kotlin/java/all)
ktmr lsp update              # reinstall any pinned server that's fallen behind the current pin
```

