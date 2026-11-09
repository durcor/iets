# Build on a remote builder

You have a derivation and a machine that should build it. `iets build`
evaluates in memory and sends the builder only what it's missing, in one go.
iets keeps no local store, so nothing has to land in `/nix/store` first.

Every flag is in [Commands](../reference/commands.md). If you want to know why
the upload works this way, read
[The transports, measured](../explanation/transports.md). Why a fetched input
needs no trust is in
[How caching works](../explanation/caching.md).

The examples build `greeting.nix` from
[Get started with iets](../tutorial/getting-started.md). It reads
`builtins.currentSystem`, which a pure evaluation doesn't have, so the
commands add `--impure`.

## Builders

`--builder URL` takes these forms:

```sh
iets build --impure --builder daemon greeting.nix
iets build --impure --builder unix:///nix/var/nix/daemon-socket/socket greeting.nix
iets build --impure --builder ssh-ng://builder.example.org greeting.nix
iets build --impure --builder grpc://farm.example.org:50051 greeting.nix
```

- `daemon` is this machine's socket, `/nix/var/nix/daemon-socket/socket`.
- `unix:///path` is any other socket that speaks the Nix worker protocol.
- `ssh-ng://[user@]host` runs `ssh host nix-daemon --stdio` and talks to it
  over its standard input and output. It reads `NIX_SSHOPTS`. To run
  another program on the host, add `?remote-program=CMD` to the URL.
- `grpc://host:port` is a
  [nix-grpc-store](https://github.com/Mic92/nix-grpc-store) service. You don't
  need `nix`, a plugin or a local store.

Without the flag, iets uses the `builder` setting in your user `iets.toml`,
and if that's unset, `store` from `nix.conf`. To set it for one run without
the flag:

```sh
iets --iets-option builder ssh-ng://builder.example.org build --impure greeting.nix
```

iets refuses a daemon older than protocol 1.35.

## gRPC parameters

A `grpc://` URL takes nix-grpc-store's own query parameters. These are the
ones you'll most likely need. The rest are in
[Builders and the wire](../reference/builders.md#url-parameters):

```sh
iets build --impure --builder 'grpc://farm.example.org:50051?token-file=/run/secrets/farm-token' greeting.nix
```

| parameter | default | what it does |
|---|---|---|
| `token-file` | none | a file holding an OIDC bearer token, re-read for every call. Needs TLS |
| `client-cert`, `client-key` | none | a PEM client identity for mTLS. Both or neither |
| `system` | the `system` setting | which worker cluster the balancer routes to |
| `max-builds` | 64 | concurrent builds in flight |
| `insecure` | `false` | plaintext instead of TLS, for local testing only |

Without credentials, the first call fails, and says why:

```
error: gRPC StoreInfo on 'farm.example.org:50051' failed (Unauthenticated):
       server requires a TLS client certificate or bearer token
hint: TLS handshake failed and no client certificate was presented.
      Set 'client-cert'/'client-key' or 'token-file'.
```

A token with the usual `write` role accepts content-addressed uploads:
source trees, `builtins.toFile` texts and derivations need no signature.
A locally valid input-addressed output pinned as an input source is copied
with its runtime closure and original metadata, without rebuilding its
producer. It needs a signature by a key the service already trusts, or an
already-authorized trusted connection. An unsigned local output does not
become content-addressed merely because it is an input source; iets does
not change the destination's trust policy.

Registered sources without an evaluator-recorded origin work on ordinary
multi-user stores too, including stored `.drv` builds and streamed `eval-jobs`.
The selected native worker supplies metadata and NAR bytes on the same
connection. iets verifies the canonical NAR hash and size into its private
input cache, then uploads from that replayable accessor. A content-addressed
cache key describes the bytes only: the upload keeps the original IA or CA
path, references, deriver and signatures, and clears `ultimate`. Destination
closure members already present are not fetched or copied. A direct local
metadata query is used with filesystem bytes only when that actual-local
provider is usable; root database write access is not needed for worker-backed
copies. Neither a different daemon namespace nor a socket alias authenticates
unchecked logical filesystem bytes.

## Flake checks

Name the whole set. `iets build` builds every derivation in
it and picks the set for the current system itself:

```sh
iets build --builder 'grpc://farm.example.org:50051?max-builds=32' .#checks
```

Every check gets its own result, and one failure doesn't stop the others.
The builder doesn't rebuild anything it already has. On a gRPC builder,
`max-builds` in the URL sets how many builds run at once.

stderr ends with a one-line summary, such as `12 built, 1 failed:
checks.x86_64-linux.clippy`. The exit code is Nix's: 100 for a failed build.
For a program, `--json` puts `{"outcomes":[…]}` on stdout, one object per
check, with the end of the log for each failure.

## Uploads

iets writes nothing to a local store on the way out. It builds the closure
from what the evaluation holds in memory: the derivations, and the sources
from where the evaluation found them (the original local tree, the input
store, or a `builtins.toFile` text in memory). Anything the evaluation did
not add, such as a path pinned with `builtins.storePath`, is read from
`/nix/store` when possible and otherwise left to the daemon
([Builders and the wire](../reference/builders.md#what-iets-sends)). Before
it sends a source, iets checks that the tree still hashes to the planned
store path, and fails the build if it changed.

To watch the upload, add `--timings`:

```sh
iets build --impure --builder daemon --no-build --timings greeting.nix
```

After the `cache:` and `eval:` lines you get one line for each step of the
upload:

```
plan: 0 sources, 1 drvs, 0.000s
connect: protocol 1.38, trusted, 0.055s
query: 1 paths, 0 missing, 0.000s
add: 0 paths, 0 bytes, 0.000s
worker: 2 ops, 5 round trips, 240 bytes sent
```

`query` counts the paths iets asked about. It asks about the roots first,
and goes down the graph only below a missing one, because a daemon that
holds a path holds all it references. `add` is one `AddMultipleToStore` of
what the answers said was missing.

## Upload only

`--no-build` plans, asks and uploads, then stops before the build:

```sh
iets build --impure --builder daemon --no-build greeting.nix
```

It prints nothing when it succeeds. Add `--timings` to see the phases. Use it
to warm up a builder, or to check that a closure uploads cleanly before you
start the build.

`--dry-run` stops even earlier and tells you what *would* happen. Copy
`greeting.nix` to `farewell.nix` and change the name and message, so the
builder hasn't built it yet:

```sh
iets build --impure --builder daemon --dry-run --json farewell.nix
```

```json
{"downloadSize":0,"narSize":0,"unknown":[],"willBuild":["/nix/store/y8g88wvqxf4chh9hggwjmdrv0mqvgq8f-farewell.drv"],"willSubstitute":[]}
```

The plan is also on stderr as text, with or without `--json`:

```
this derivation will be built:
  /nix/store/y8g88wvqxf4chh9hggwjmdrv0mqvgq8f-farewell.drv
```

The JSON is byte for byte what `iets plumbing missing` prints
([Plumbing operations](../reference/plumbing.md)). If the builder already has
every output, as it does for `greeting.nix`, all the lists are empty.

Even with `--dry-run` or `--no-build`, the daemon may substitute a source it
needs, because it can't register the derivations without it.

## Outputs

A remote builder keeps its outputs where it built them. To get them,
`--fetch-to DIR` streams each wanted output back into `DIR`:

```sh
iets build --impure --builder daemon --fetch-to out greeting.nix
ls out/
```

```
hl2d12ai352g88ad47fh8844g939iyrd-greeting
```

Each output lands under the base name of its store path. Nothing is
registered with a local daemon, so you don't need to be a trusted user.

To build only one output, use `--out NAME`. Repeat it to pick several
outputs of one derivation:

```sh
iets build --impure --builder daemon --out out -I nixpkgs=$nixpkgs -E '(import <nixpkgs> {}).hello'
```

```
/nix/store/xl1h9i29pgq2q5cszjhm5wpfxfbbqwyi-hello-2.12.3
```

## Build a `.drv`

`iets build` also takes a `.drv` path. The closure then comes from
`/nix/store` instead of an evaluation:

```sh
iets build --builder daemon /nix/store/6jq7mz1b1n11y522nzjdqyvr3gwb69zz-greeting.drv
```

If an input source came from a local tree that the store does not have,
tell iets where to find it:

```sh
iets build --builder daemon --source /nix/store/…-src=./src /nix/store/…-thing.drv
```

What iets can't do over a transport is listed in
[Known limits](../reference/builders.md#known-limits).
