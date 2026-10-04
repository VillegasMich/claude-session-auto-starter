# Repository setup

One-time settings for the GitHub repository so CI, the Docker image and releases work. Nothing
here is needed to *run* the service on your own machine ([deployment.md](deployment.md) covers
that).

| What                          | Where                                   | Needed for                          |
| ----------------------------- | --------------------------------------- | ----------------------------------- |
| `DOCKERHUB_USERNAME` variable | Settings → Secrets and variables → Actions → *Variables* | Publishing the image on release |
| `DOCKERHUB_TOKEN` secret      | Settings → Secrets and variables → Actions → *Secrets*   | Publishing the image on release |
| `DOCKERHUB_IMAGE` variable    | same, *Variables* (optional)            | Image name other than `<user>/claude-session-starter` |
| Workflow permissions          | Settings → Actions → General            | Release workflow pushing the version bump |
| Branch ruleset on `main`      | Settings → Rules → Rulesets             | Requiring CI before merging         |

Without any of them CI still runs: fmt, clippy, tests, shellcheck, the Docker build and its smoke
test. Nothing is pushed.

CI never needs a Claude token: no job sends a message or calls the usage endpoint. The only
end-to-end check is manual (`cargo run -- status`, `cargo run -- once`; see
[CLAUDE.md](../CLAUDE.md#testing-notes)).

## What CI does

[`ci.yml`](../.github/workflows/ci.yml):

| Job      | Runs on         | Does                                                                     |
| -------- | --------------- | ------------------------------------------------------------------------ |
| `test`   | every push / PR | `cargo fmt --check`, `clippy -D warnings`, `cargo test --locked`, `shellcheck scripts/*.sh` |
| `docker` | every push / PR | Builds the image, smoke test, dry-run push. On a published release: multi-arch (`amd64`, `arm64`) push to Docker Hub. |

The smoke test runs offline with a dummy token: `claude-session-starter --version`,
`claude --version` (the pinned Claude Code in the image), and `start --dry-run`, which must print
the minimal starter command with the token `[redacted]` and nowhere in clear.

[`release.yml`](../.github/workflows/release.yml): manual; bumps the version, creates the GitHub
release and triggers the image publish ([Releasing](#releasing)).

[`dependabot.yml`](../.github/dependabot.yml): weekly update PRs for crates, Actions and the
Docker base images. Claude Code in the image is pinned by `ARG CLAUDE_CODE_VERSION` in the
`Dockerfile` and bumped by hand.

## Docker Hub (publishing the image)

The `docker` job pushes when a GitHub release is **published**, or when CI is run manually on a
release tag with *publish* (what the release workflow does). Tag `v1.2.3` becomes image tags
`1.2.3`, `1.2` and `latest` (no `latest` for pre-releases like `v1.3.0-rc.1`).

1. Docker Hub → *Account settings* → *Personal access tokens* → *Generate new token*.
   Description `github-actions claude-session-starter`, access **Read & Write**, an expiration you
   will remember. Copy it (shown once).
2. Optional: create the repository `claude-session-starter` on Docker Hub first to choose its
   visibility; otherwise the first push creates it (public on free plans). The image contains no
   token or account data, only the binary and Claude Code.
3. In the GitHub repository:

   ```bash
   gh secret set DOCKERHUB_TOKEN                       # paste the token at the prompt
   gh variable set DOCKERHUB_USERNAME --body <docker-hub-user>
   gh variable set DOCKERHUB_IMAGE --body <org>/claude-session-starter   # optional
   ```

   Or in the web UI: *Settings* → *Secrets and variables* → *Actions*.

The job fails with a clear error if the secret or variable is missing when it needs them. Only
the publish path logs in, and secrets are never exposed to pull requests from forks.

The published image is the same one `scripts/install.sh` builds locally. To use it:
`IMAGE=<docker-hub-user>/claude-session-starter:<version> scripts/install.sh`, or replace
`claude-session-starter` with it in the `docker run` command of
[deployment.md](deployment.md#running).

## Actions permissions

*Settings* → *Actions* → *General*:

- *Actions permissions*: allow GitHub Actions and reusable workflows (the workflows use
  `actions/*`, `docker/*`, `dtolnay/rust-toolchain`, `Swatinem/rust-cache`).
- *Workflow permissions*: **Read repository contents** is fine as the default; each workflow
  declares what it needs (`release.yml` asks for `contents: write` and `actions: write`).

## Protecting `main`

*Settings* → *Rules* → *Rulesets* → *New branch ruleset*, target `main`:

- Require a pull request before merging.
- Require status checks: `fmt, clippy, tests`, `Docker image (smoke test, push on release)`.
- Block force pushes.
- **Bypass list:** add the *GitHub Actions* app (or *Repository admin*), otherwise the release
  workflow can't push its version-bump commit to `main`.

## Releasing

### From GitHub Actions (recommended)

*Actions* → **Release** → *Run workflow* on `main`, or:

```bash
gh workflow run release.yml                       # bump from commit types (auto)
gh workflow run release.yml -f bump=minor
gh workflow run release.yml -f version=1.0.0-rc.1
gh workflow run release.yml -f dry_run=true       # only show what would happen
```

The workflow:

1. Checks that CI passed on the commit.
2. Computes the version with [`scripts/bump-version.sh`](../scripts/bump-version.sh): if
   `Cargo.toml`'s version is already released, bumps it from the Conventional Commits since the
   last tag (`feat`/`chore` → minor, breaking → major, else patch; breaking is minor while 0.x).
   The first run releases the current `Cargo.toml` version (`0.1.0`) as is.
3. Commits the bump to `main`, creates tag + release with
   [`scripts/release.sh`](../scripts/release.sh) (generated notes).
4. Runs CI on the tag with *publish* and waits until the image is on Docker Hub.

It uses the built-in `GITHUB_TOKEN` (no extra secret) plus the Docker Hub settings above.

### Locally

From an up-to-date, clean `main` (`gh` logged in):

```bash
scripts/bump-version.sh minor && cargo build && git commit -am "chore(release): bump version to $(scripts/bump-version.sh)" && git push
scripts/release.sh --dry-run     # checks only
scripts/release.sh               # tag + GitHub release; CI then publishes the image
```

## Checklist

```bash
gh secret set DOCKERHUB_TOKEN
gh variable set DOCKERHUB_USERNAME --body <docker-hub-user>
gh secret list && gh variable list
```

Then push to `main` and check the *Actions* tab: `test` and `docker` should be green, and the
`docker` job summary lists the tags a release would get.
