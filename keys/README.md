# Device keys (OTA + Tailscale)

## OTA signing

**Public key** (committed, shipped on devices):

```
keys/update_public.pem
```

The `vesyl-print` binary compiles it in, and release tarballs carry it.
`setup.sh` also installs it as `/etc/vesyl-print/keys/update_public.pem`, for
configs whose `update_public_key_path` points there.

**Private key** (never commit): store as GitHub Actions secret **`UPDATE_PRIVATE_KEY`**
(full PEM, including `BEGIN`/`END` lines). Only the `sign` job of
`.github/workflows/release.yml` gets it.

## Tailscale auth key (factory)

```
keys/tailscale.key
```

- **Never commit.** Used only by `setup.sh` on first provision: copy it into
  `keys/` of the extracted release before running `setup.sh`.
- Contents: a **one-time** Tailscale auth key (single line).
- `setup.sh` installs Tailscale (if needed) and runs:

```bash
tailscale up \
  --hostname="$(hostname)" \
  --auth-key="$(cat keys/tailscale.key)"
```

- On **successful** join, `setup.sh` **deletes** the key file (one-time use).
  On failure the file is left so you can retry.
- After a full factory setup succeeds, `setup.sh` also **removes the entire
  source tree** used to provision (the extracted release; the app runs from
  `/opt/vesyl-print/current`). Lab: `sudo SKIP_SOURCE_CLEANUP=1 ./setup.sh`.
- Not copied into `/opt/vesyl-print/releases/*` (OTA slots) or release tarballs.
- Skip Tailscale: `sudo SKIP_TAILSCALE=1 ./setup.sh`
- Override path: `sudo TAILSCALE_AUTH_KEY_FILE=/path/to.key ./setup.sh`

Pass these options after `sudo`: it drops variables set before it.

## Generate a new key pair

```bash
openssl genpkey -algorithm Ed25519 -out update_private.pem
openssl pkey -in update_private.pem -pubout -out keys/update_public.pem
# Add update_private.pem contents to GH secret UPDATE_PRIVATE_KEY, then delete local private file
```

## Build a release locally

```bash
UPDATE_PRIVATE_KEY_FILE=./update_private.pem ./scripts/build-release.sh 0.4.0
VERIFY_ONLY=1 ./scripts/build-release.sh 0.4.0   # the publish job's check
# artifacts in dist/
gh release create v0.4.0 dist/* --generate-notes
```

Or push a tag and let CI do it:

```bash
git tag v0.4.0
git push origin v0.4.0
```

## Canonical signature

`scripts/build-release.sh` signs the manifest's canonical JSON: every field
except `signature` and nulls, keys sorted, compact separators (`,` / `:`),
non-ASCII escaped as `\uXXXX`. It builds it with `jq -S -c -a`, which gives the
same bytes as Python's `json.dumps(sort_keys=True, separators=(",", ":"))`.
Devices rebuild the same form in `update.rs` `ReleaseManifest::canonical_bytes()`
and verify with Ed25519; the Rust tests in
`rust/crates/vesyl-print/tests/build_release.rs` check that the two agree.
