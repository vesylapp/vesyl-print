# Update signing keys

**Public key** (committed, shipped on devices):

```
keys/update_public.pem
```

**Private key** (never commit): store as GitHub Actions secret **`UPDATE_PRIVATE_KEY`**
(full PEM, including `BEGIN`/`END` lines). Used by `.github/workflows/release.yml`.

## Generate a new key pair

```bash
openssl genpkey -algorithm Ed25519 -out update_private.pem
openssl pkey -in update_private.pem -pubout -out keys/update_public.pem
# Add update_private.pem contents to GH secret UPDATE_PRIVATE_KEY, then delete local private file
```

## Build a release locally

```bash
UPDATE_PRIVATE_KEY_FILE=./update_private.pem ./scripts/build-release.sh 0.4.0
# artifacts in dist/
gh release create v0.4.0 dist/* --generate-notes
```

Or push a tag and let CI do it:

```bash
git tag v0.4.0
git push origin v0.4.0
```

## Canonical signature

`scripts/build-release.sh` signs compact JSON of the manifest **without** the
`signature` field (`sort_keys=True`, separators `,` / `:`). Devices verify the
same form in `update.ReleaseManifest.canonical_bytes()`.

Ship `update_public.pem` on appliances (this directory or
`/etc/vesyl-print/keys/update_public.pem` via `setup.sh`).
