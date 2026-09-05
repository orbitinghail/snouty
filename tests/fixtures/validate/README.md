# validate sample projects

Small Antithesis config directories used to exercise `snouty validate` (and, by
hand, `snouty launch`) across the situations it has to handle. The gallery
generator (`scripts/gen-gallery.py`) runs `snouty validate` against each of
these to produce the `validate-*` gallery stories.

**The goal of each sample is documented in the comment at the top of its
`docker-compose.yaml`** — read that to understand what the sample is and what
outcome to expect. (The `neither/` sample has no compose file, so its
explanation lives in `neither/README.md` instead.)

**A container runtime is required for all of these.** `snouty validate` resolves
docker/podman (and shells out to `docker-compose`) before it inspects a config,
so even the static checks need the runtime binaries installed.

At a glance:

**Static misconfigurations** (fail before any container starts; need the
docker/podman + docker-compose binaries, but not a running daemon):
`neither/`, `wrong-extension/`, `ambiguous/`, `malformed-compose/`,
`no-services/`, `external-network/`.

**Need a running Docker daemon:**
- `missing-image/` — queries the local image store to report the missing image.
- Live runs that start real containers: `valid/`, `timeout/`,
  `unrecognized-command/`, `non-executable-command/`, `stranded/`.

All service images use the glibc-based `debian:bookworm-slim` — Antithesis does
not support non-glibc (e.g. musl/Alpine) images. The live samples **bake their
test commands into an image** (via a `Dockerfile` + compose `build:`) rather
than bind-mounting them, so the same image works for `validate` and `launch`.

## Build the sample images first

`snouty validate` never builds or pulls, so the live samples' images must exist
locally beforehand. Build them all:

```sh
scripts/build-validate-samples.sh
```

`missing-image/` deliberately has no Dockerfile — it references an image that
doesn't exist, to exercise validate's "image not available locally" path — so
it is skipped by the build script.

## Validate

```sh
snouty validate tests/fixtures/validate/valid
```

The `timeout/` sample never emits the setup-complete event, so give it a short
deadline instead of waiting the full default:

```sh
snouty validate tests/fixtures/validate/timeout --timeout 5
```

## Launch

`validate` runs a config locally; `launch` submits it to Antithesis. `--config`
points at the directory, which snouty builds and pushes as the config image —
the compose service images must already exist locally (snouty pulls only an image below a `private_registries` prefix).
Set your tenant, repository, and credentials first (see `snouty launch --help`).

```sh
ANTITHESIS_TENANT=your-tenant \
ANTITHESIS_REPOSITORY=us-central1-docker.pkg.dev/your-proj/your-repo \
ANTITHESIS_API_KEY=… \
  snouty launch \
    --webhook basic_test \
    --config tests/fixtures/validate/valid \
    --test-name "snouty-valid-sample" \
    --description "snouty valid sample harness" \
    --duration 15 \
    --ephemeral
```
