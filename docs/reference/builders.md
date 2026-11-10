# Builders and the wire

`iets build` sends a build somewhere. This page covers where, what it
sends, and the two protocols it speaks: the Nix worker protocol and
`grpc://`. Both carry NARs.

`iets build` evaluates in memory, works out the closure of the root
derivations, and sends only what the store lacks.
`docs/explanation/transports.md` says why and has the measured numbers, and
`docs/how-to/build-remotely.md` has the steps.

## The builder URL

```
--builder daemon
--builder unix:///path
--builder ssh-ng://[user@]host[?remote-program=CMD]
--builder grpc://host:port[?k=v&…]
```

Without the flag, the URL is the `builder` setting in your user
`iets.toml`, or nix.conf's `store` when that is unset
(`docs/reference/settings.md`). A classic `--store URL` or
`--option store URL` wins over both.

- `daemon`, `auto` and an empty value mean
  `/nix/var/nix/daemon-socket/socket`.
- `ssh-ng://` runs `ssh host nix-daemon --stdio` and talks over its stdio,
  as Nix's `ssh-ng` store does. It reads `NIX_SSHOPTS`.
- `grpc://` is a protocol of its own, described under "`grpc://`".

Any other store URL is the usage error `unsupported builder 'URL':
expected daemon, unix:///path, ssh-ng://[user@]host or grpc://host:port`.
In classic mode, iets falls back to the real nix.

## What iets sends

- **Every derivation the evaluation made**, as the exact ATerm it hashed.
  An input derivation the evaluation did not make came from a
  `/nix/store/*.drv`, reached through string context or `import`, and iets
  reads that file.
- **Every source the evaluation added**, dumped again from where the
  evaluation read it: a local tree with the entries a `filterSource`
  filter accepted, a fetched input, or a `builtins.toFile` text held in
  memory. After hashing, iets checks that the NAR still gives the planned
  store path. If the tree changed since evaluation, the build fails.
- **Sources the evaluation did not add** (under a derivation read from the
  store, or a path pinned with `builtins.storePath`) are queried with
  `nix path-info --json`. A locally valid path is copied with its original
  identity, NAR hash and size, references, deriver, signatures and content
  address, including an input-addressed build output whose `ca` is null.
  iets follows runtime references, not the producer derivation or its build
  inputs, and asks the destination before locating each new reference.
  Already-valid members need no local metadata or bytes. References register
  before referrers; a self-reference registers only once. Only a path that
  is genuinely unavailable locally is left to the builder's substituters.
  Malformed metadata is an error, not a substitution miss.

iets walks the closure once and reads nothing during the walk. It asks the
daemon which paths it has, then finds and hashes only the sources the daemon
lacks. Each path goes after every path it references. `iets plumbing plan`
prints the result of that walk (`docs/reference/plumbing.md`).

### The `DRV` form

```
iets build [--source STOREPATH=LOCAL]... DRV
```

`DRV` is a `.drv` store path, or a copy of one named `<hash>-<name>.drv`.
The root comes from that file, and its closure from `/nix/store`.
`--source STOREPATH=LOCAL` names the original local tree of an input
source.

### Job and core limits

For `daemon` and `unix://`, `iets build -j1 --cores 1 .#pkg` sets one
concurrent local build and one core per build. `-j N`, `-jN` and
`--max-jobs N` set `max-jobs`; `--cores N` sets `cores`. The defaults come
from nix.conf, including user files and `NIX_CONFIG`, then command-line
flags and `--option` pairs apply in order: the last setting wins. Aliases
such as `--option build-max-jobs 1` and `--option build-cores 1` participate
in the same order. A cached evaluation still uses the current limits.

Jobs accept a bounded unsigned integer or `auto` (the CPU count). Jobs `0`
forbids local builds, but already-built outputs can still be used. Cores
accept a bounded unsigned integer; `0` means all available cores, not a
hard CPU or memory cap. Builders receive `NIX_BUILD_CORES` and must honor
it. Both integer settings are bounded by 4294967295. `--workers` controls
evaluation threads, not build jobs or per-build cores.

`ssh-ng://` deliberately sends no `SetOptions`: the remote service's own
resource policy applies. `grpc://` has no `SetOptions` or cores field; its
URL's `max-builds` limits concurrent calls. Configured client-side jobs and
cores do not limit either transport. Native `iets build` rejects explicit
job/core flags and `--option` forms on these transports before connecting,
rather than claiming to enforce them. Classic mode's existing behavior is
unchanged: SSH uses remote settings and gRPC warns about unsent settings.


## Transports

`iets build`, classic `nix-build` and `nix build` build the same way over
every transport. `nix-instantiate`, `nix-shell` and `print-dev-env` upload
the same way. `iets eval-jobs` uploads in batches while the walk runs.

| | worker protocol (`daemon`, `unix://`, `ssh-ng://`) | `grpc://` |
|---|---|---|
| a ready set | one `BuildPathsWithResults` | a `Want` on the `Schedule` stream and one `BuildDerivation` per derivation of the set's closure, `max-builds` in flight |
| build parallelism | daemon/unix: `-j` through `SetOptions`; ssh-ng: remote policy | the client's URL `max-builds`, in flight, placed by the scheduler |
| log lines | attributed by the daemon's activity ids | by the call, which knows its derivation |
| output paths | `QueryDerivationOutputMap` | statically, from the derivations |
| out-links | the local daemon, the closure copied back first | the local daemon substitutes from its caches; the rest by `FetchNars` |

The stdout of classic mode is byte for byte the real nix's
(`tests/classic_*.rs`).

## The worker protocol

iets's client is written from the C++ source of Nix 2.34.8, with no Nix
library.

### Handshake

The same as Nix's `initConnection`: `WORKER_MAGIC_1`, iets's protocol
version (1.38), the daemon's version, a feature-set exchange at >= 1.38
(iets offers none), the obsolete CPU-affinity and reserve-space words, then the
daemon's `nix_version` and its trust flag, then one `STDERR_LAST`.
**iets refuses daemons below protocol 1.35.** No features are negotiated.

### `SetOptions`

Over `daemon` and `unix://`, iets sends the fields Nix's
`RemoteStore::setOptions` sends, with these values:

| Option | Value |
|:---|:---|
| keep-going | 1 with `--keep-going` or more than one root, else 0 |
| max-jobs | configured `max-jobs`, overridden by job flags/`--option`; default `auto` (CPU count), 0 forbids local builds |
| build verbosity | `lvlVomit`, so builder output arrives as results attributed to their build activity |
| substitutes | on |
| cores | configured `cores`, overridden by `--cores`/`--option`; default 0 (all cores) |
| keep-failed, try-fallback, max-silent-time | 0 |
| the override map | empty |

In classic mode, these come from nix.conf instead: `keep-failed`,
`keep-going`, `fallback`, `max-jobs`, `max-silent-time`, `cores` and
`substitute`. The override map then holds every other store setting you
set outside the system nix.conf (`docs/reference/settings.md`).

Over `ssh-ng://`, iets sends no `SetOptions`, as Nix's `SSHStore` does
(ssh-store.cc). The remote daemon's own settings apply. nixbuild.net
rejects the op.

### Requests

1. **`QueryValidPaths`** on the roots, with the substitute flag off. When
   every root's wanted outputs have a static path (input-addressed or
   fixed), and the daemon holds every root, a second `QueryValidPaths`
   asks about those outputs. If the daemon holds them all, the root is
   built: iets skips steps 2 to 6 and reports the paths.
2. **`QueryValidPaths`** on the rest of the plan, top-down, with the
   substitute flag off. A daemon holds the closure of every path it holds,
   so a held derivation answers for everything under it. iets asks about
   the references of the missing roots in one call, then about whatever is
   still open in one last call. A warm daemon answers in one or two small
   calls; a cold one costs two more round trips than one call over the whole
   plan. Then, off the wire, iets finds and hashes the missing native sources:
   what the evaluation read and the `--source` overrides first. Other sources
   retain their stored metadata from `nix path-info --json` batches, with
   further destination queries and metadata batches for newly found references.
   The flake fetcher uses the
   same request with the substitute flag on, for a locked input missing
   from the eval store (`docs/reference/flakes.md`).
3. **`BuildPathsWithResults`** on the missing sources iets cannot produce
   (plain store paths, which the daemon substitutes), before any upload.
   The daemon rejects a path whose reference is not valid, so it must
   hold these before the derivations that name them. Skipped when there
   are none. A failed substitution ends the run with the daemon's message.
   `nix build` does the same for an `inputSrcs` entry it does not own.
4. **`AddMultipleToStore`** of the missing paths, in one framed stream, with
   references before referrers. Each entry is a `ValidPathInfo` in the 1.16
   layout the daemon expects, followed by its NAR. Stored paths retain their
   original metadata and signatures, but `ultimate` is cleared: copying is
   not a local build. For native evaluation objects, iets writes no deriver,
   its own NAR hash and size, registration time 0, `ultimate = false`, no
   signatures, and the content address, which the daemon checks:

   | Path | Content address |
   |:---|:---|
   | a source tree | `fixed:r:sha256:<nar hash>` |
   | a `builtins.toFile` path | `text:sha256:<flat hash>` plus references |
   | a `.drv` | `text:sha256:<sha256(aterm)>`, references = inputSrcs + inputDrvs |

   The daemon accepts a content-addressed path from an untrusted client
   without a signature. An input-addressed source needs a signature by a key
   the destination already trusts, or an already-trusted connection. iets
   does not change that authorization or manufacture a content address.
5. **`BuildPathsWithResults`** in normal mode on `<root drv>!<out1,out2>`
   (`!*` without `--out`), the `DerivedPath` string that protocol >= 1.30
   reads, one entry per root in the same request. A failed status is that
   root's verdict. `--no-build` and `--dry-run` stop before this step. The
   substitution of step 3 still runs, or the derivations could not
   register.
6. **`QueryDerivationOutputMap`** for each root that built and has an
   output with no static path: its output paths.
7. **`NarFromPath`** for each wanted output with `--fetch-to DIR`,
   restored into `DIR`.

### Events and timings

A daemon's `STDERR_NEXT` is an unattributed log line. A
`STDERR_START_ACTIVITY` of type `actBuild` maps the activity id to its
derivation, and a build log result becomes a log line attributed through
that map. Under `internal-json`, or with a `json-log-path`, every activity
message also goes to the log (`docs/reference/errors.md`).

`--timings` prints one line per step on stderr: `plan`, `connect`, `query`,
`resolve` (finding and hashing the missing sources), `substitute` (only when
paths were left to the daemon), `add`, `build`, `outputs`, `fetch` (only with
`--fetch-to`), `link` (only with `-o`: copying the outputs into the local
store and adding the GC roots), and a `worker`
line with the round trips.

## `grpc://`

[nix-grpc-store](https://github.com/Mic92/nix-grpc-store) is a gRPC
service in front of a Nix store. iets speaks it directly, so
`--builder grpc://…` and `--store grpc://…` need no nix, no plugin and no
local store.

### URL parameters

`grpc://host:port?k=v&…` takes nix-grpc-store's own parameter names and
defaults:

| parameter | default | what it does |
|---|---|---|
| `system` | the `system` setting | `x-nix-system` on every call: which worker cluster the balancer routes to |
| `token-file` | none | file with a Keycloak/OIDC bearer token, re-read for every call. Needs TLS |
| `client-cert`, `client-key` | none | PEM client identity for mTLS. Both or neither |
| `ca-cert` | none | PEM CA trusted in addition to the usual roots (`NIX_SSL_CERT_FILE` in `docs/reference/settings.md`) |
| `insecure` | `false` | plaintext instead of TLS. Local testing only |
| `connect-timeout` | 30 | seconds the first call waits for the service |
| `reschedule-retries` | 8 | how often a build a worker bounced goes back to the scheduler |
| `restart-grace` | 120 | seconds to wait for a scheduler that went away, or for a worker that can take an `Unplaceable` derivation |
| `max-builds` | 64 | concurrent `BuildDerivation` calls; nix.conf `max-jobs` does not apply |
| `nar-connections` | 4 | accepted for URL compatibility; this client fetches over its one channel |

For example:

```
--builder 'grpc://farm.example.com:443?token-file=/run/secrets/token&max-builds=128'
```

### Authentication

| Header | Value |
|:---|:---|
| `authorization` | `Bearer <token>` from `token-file`, read fresh for each call, so a rotating projected volume or a CI refresher works |
| `x-nix-system` | on every call, so an upload and the build that follows land on the same cluster. A `BuildDerivation` overrides it with the derivation's own `platform` |
| `x-nix-worker` | on a `BuildDerivation`: the address the scheduler assigned, which the balancer routes to |
| `x-nix-features` | on a `BuildDerivation`: the derivation's `requiredSystemFeatures`, comma separated |

### Step by step

First, one `StoreInfo` call, for the trust flag. Then:

1. `QueryValidPaths` with base names, top-down from the roots as on the
   worker protocol. Then iets finds and hashes the missing sources, as on
   the worker protocol.
2. The sources iets cannot produce stay with the store. A worker
   substitutes an input when the build asks for it, so iets only checks
   that some substituter has them, which `QueryValidPaths` with
   `substitute` answers.
3. `AddMultipleToStore`: one client-streamed zstd stream that decompresses
   to the worker-protocol framing the `unix://` path writes.
4. `QueryValidPaths` on output paths, top-down from the roots: first the
   roots' outputs, then the outputs of the input derivations of each
   derivation that has a missing output. After two levels, iets asks about
   everything left in one call. Nothing below a built output is built.
5. For each derivation, with at most `max-builds` in flight, and only once
   every input derivation of it is done: a `Want` on the batch's one
   `Scheduler.Schedule` stream. The scheduler answers `Cached` (the verdict
   is `AlreadyValid`), `Assigned` (a worker address), or `Unplaceable`, which
   is only advice: the `Want` stays queued, and fails after `restart-grace`.
   On `Assigned`, a `BuildDerivation` goes to that worker. Its log lines
   and phases become `Log` events, and its result becomes the derivation's
   verdict. A derivation whose outputs are already valid (step 4) is
   skipped, so it costs no `Want`.
6. `--fetch-to DIR`: the farm never copies an output into a local store, so
   `FetchNars` streams the NARs back, and iets restores each into `DIR`.

iets never opens the worker-protocol tunnel (`Connect`), which a farm
endpoint refuses with `UNIMPLEMENTED`. That is why `nix` needs
`--eval-store auto` there and iets doesn't.

### `write` tokens

`write`, the role an OIDC token usually gets, may request builds and import
**signed** paths. Native source trees, `builtins.toFile` texts and derivations
are content-addressed, so the service accepts them without signatures.
Locally built input-addressed sources keep their original signatures and
need a key the service already trusts. Unsigned input-addressed paths need
an already-authorized `trusted` role. iets neither grants that role nor
changes the service's trust settings.

Outputs land in the farm's binary cache. `--fetch-to` restores them into a
directory. Without an out-link (`iets build -o`, or `result` under the
classic mode), nothing comes back. An out-link first asks the local daemon to substitute the outputs
(`QueryValidPaths` with `substitute`), which fetches them from the farm's
cache when `nix.conf` lists it. A worker's own path infos carry only the
worker's key, which isn't the cache key. What the local daemon cannot
substitute is read back with `QueryPathInfos`, fetched with `FetchNars`, and
registered with one worker-protocol `AddMultipleToStore` that carries each
path's signatures. The daemon checks them, as `nix copy --from` does. So your
local `nix.conf` must list and trust the cache (`substituters`,
`trusted-public-keys`), or you must be a trusted user.

### Retries

gRPC reports every connect failure as `UNAVAILABLE`.

| What | Policy |
|:---|:---|
| a unary call | 500 ms doubling to 4 s, until `connect-timeout` has passed |
| the `Schedule` stream | reconnects when the scheduler says `Restarting`, or goes away with `UNAVAILABLE`, `UNKNOWN` or `INTERNAL`, for up to `restart-grace`, and sends every unanswered `Want` again |
| a bounced `BuildDerivation` (`ABORTED`, `FAILED_PRECONDITION`, `UNAVAILABLE`, `UNKNOWN`, `INTERNAL`) | back to the scheduler, up to `reschedule-retries` times. Unlike nix-grpc-store's own client, each ask after the first waits: 1 s doubling to 30 s |
| `NOT_FOUND` | the worker has not got the `.drv` closure: iets uploads it (`QueryValidPaths` then `AddMultipleToStore`, with the same routing headers) and asks once more |
| an `Assigned` naming an address that bounced twice in a row | refused without a call: the derivation goes back to the scheduler once the address's cooldown has passed, bounded by `restart-grace`, not by `reschedule-retries` |

All builds of one run share what they learn about an address: two bounces
in a row close it, and a build that returns a result on it opens it again
(`docs/explanation/transports.md` says why). When no worker is left, the
error names the last address.

## NAR

The NAR iets writes is byte-identical to `nix-store --dump`. The tracing
cache keeps the NAR hash of each copied tree, and dumps the tree again only
when its metadata changed (`docs/explanation/caching.md`).
`docs/explanation/transports.md` says how this is tested and how fast it is.

## Known limits

- `ssh-ng://` is tested through a local `ssh` shim, and against
  nixbuild.net with `bench/nixbuild.nu`. There is no `--ssh-option` passthrough: use
  `NIX_SSHOPTS` or `~/.ssh/config`.
- Over `daemon` and `unix://`, `iets build` sends `max-jobs` equal to the
  CPU count. It has no flag for that or for substituters. A `grpc://` builder always keeps going, so `-k`
  changes nothing there.
- iets asks for the output paths of the roots only. A dependency that built
  gets no line of its own beyond its result.
- During a large upload, an error shows only after the whole stream is
  sent, because iets does not read the daemon's stderr while it streams.
- Over `grpc://`, ^C does not cancel the build on the server.
