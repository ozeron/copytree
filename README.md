# Copytree

Fast git worktrees for parallel coding agents.

Every workspace is a normal linked `git worktree`, so git, your IDE and every agent harness see it unchanged. Copytree adds the part worktrees never solved: a new worktree arrives with its dependencies installed, its build caches warm and its `.env` in place, in seconds and without duplicating gigabytes on disk.

```console
$ copytree new fix-login
copytree: checkout: git worktree add -b ct/fix-login ../.copytree/app/fix-login 4f775cd
copytree: volume: copy-on-write clones available
copytree: carry: .env
copytree: node: running `pnpm install --frozen-lockfile --prefer-offline`
copytree: python: running `uv sync --frozen`
copytree: seed: cloned .next/cache (1832 files)
/home/me/src/.copytree/app/fix-login

$ copytree list
NAME       BRANCH        STATUS  PATH
fix-login  ct/fix-login  ready   /home/me/src/.copytree/app/fix-login

$ copytree rm fix-login
removed fix-login (restore with `git checkout -b <branch> refs/copytree/fix-login`)
```

## What `copytree new` does

1. **Admit.** Refuse when the pool's volume has less than 1 GiB free.
2. **Check out** with `git worktree add -b ct/<name>` into the pool, `../.copytree/<repo>/<name>` by default. The pool sits on the repo's volume (needed for copy-on-write) and outside the repo (so file watchers ignore it).
3. **Carry** small untracked config: `.env`, `.env.local`, and anything listed in `.worktreeinclude`. A file that isn't gitignored is refused unless `--force-carry`, so a carried secret never sits one `git add -A` away from a commit.
4. **Install dependencies** from each package manager's shared store, which links or clones instead of downloading:

   | Project has | Copytree runs |
   |---|---|
   | `pnpm-lock.yaml` | `pnpm install --frozen-lockfile --prefer-offline` |
   | `bun.lock` | Same as npm, with `bun install --frozen-lockfile` |
   | `package-lock.json` | CoW-clone main's gitignored `node_modules` folders when the lockfile matches main and the volume supports it, else `npm ci --prefer-offline` |
   | `yarn.lock` (classic) | Same as npm, with `yarn install --frozen-lockfile --prefer-offline` |
   | `yarn.lock` + `.yarnrc.yml` (Berry) | `yarn install --immutable` |
   | `uv.lock` | `uv sync --frozen`, with `UV_LINK_MODE=clone` on CoW volumes and `hardlink` elsewhere |
   | `requirements.txt` | `uv venv` then `uv pip sync requirements.txt` |
   | `poetry.lock` / `pixi.lock` | `poetry install` / `pixi install --frozen` |

   `.venv` is **never** cloned. Its scripts and the project's own editable install point at the main checkout, so an agent's tests would silently run main's code instead of its edits. The e2e test checks this.
5. **Seed caches** with copy-on-write: `.next/cache`, `.turbo`, `.nx/cache`, `.parcel-cache`, `.vite`, `node_modules/.cache`, `*.tsbuildinfo`, `.mypy_cache`, `.ruff_cache`, `.pytest_cache`, `target`. Patterns match nested packages too (`apps/web/.next/cache`). Modification times are preserved so incremental builds stay valid. On a volume without copy-on-write, a cache is copied only when it is under `copy_limit_mib` (64 MiB).
6. **Run `post_create` hooks**, such as `pnpm prisma generate`.
7. **Record** metadata in `.git/copytree/`, outside the workspace.

If any step fails, the worktree and its branch are rolled back (`--keep-on-failure` keeps them).

## Clone backends

| Platform | Mechanism |
|---|---|
| macOS APFS | One `clonefile(2)` per directory, recursive and in-kernel |
| Linux btrfs, XFS (`reflink=1`) | Per-file `FICLONE` on a thread pool |
| Windows ReFS / Dev Drive | Block cloning via `reflink-copy` |
| ext4, NTFS, anything else | Plain copy under the size cap; deps come from the package manager's store instead |

`copytree doctor` probes the pool's volume and tells you which one you get.

## Works with any worktree tool: hook mode

```console
$ copytree install-hook
```

This installs a git `post-checkout` hook. git runs it after every `git worktree add`, so worktrees made by Claude Code, VS Code, worktrunk, lazygit or plain git get the same carry, deps and seed steps. The hook chains to any existing `post-checkout` hook, is a 2 ms no-op on ordinary checkouts, and never fails your git command. `copytree uninstall-hook` restores the previous hook.

`git worktree add --no-checkout` skips post-checkout hooks, so those worktrees aren't prepared.

## Commands

| Command | |
|---|---|
| `copytree new [NAME]` | Create a workspace. `--base <commit>`, `-b <branch>`, `--checkout <existing-branch>`, `--detach`, `--path <dir>`, `--no-deps`, `--no-seed`, `--no-carry`, `--print-path`, `--json`, `--timings` |
| `copytree list` | Linked worktrees with status (`ready`, `failed`, `unmanaged`). `--json` |
| `copytree path NAME` | Print the path, for `cd "$(copytree path fix-login)"` |
| `copytree rm NAME...` | Save `refs/copytree/<name>` (a stash commit if there are uncommitted edits), rename into trash, prune, delete in the background. Refuses untracked files unless `--force`. `-D` also deletes the branch |
| `copytree doctor` | git version, clone backend, free space, detected package managers, hook status |
| `copytree config` | The effective configuration |
| `copytree install-hook` / `uninstall-hook` | Hook mode |

Every command also works as `git copytree <command>` through the `git-copytree` binary.

## Configuration

Optional `.copytree.toml` at the repo root. Everything has a default; `copytree config` prints the full set.

```toml
branch_prefix = "agent/"
pool = "../worktrees"            # relative to the repo root

[seed]
extra = ["dist", ".angular/cache"]
copy_limit_mib = 64              # plain-copy cap without copy-on-write; 0 disables

[carry]
files = [".env", ".env.local", "certs/*.pem"]
worktreeinclude = true           # also read .worktreeinclude

[deps]
mode = "auto"                    # or "off"
# node = "pnpm install --offline"   # override the detected command
# python = "uv sync --frozen --all-extras"

[hooks]
post_create = ["pnpm prisma generate"]
```

## Library

The CLI is a thin layer over the `copytree` crate, so orchestrators can embed it:

```rust
let repo = copytree::git::Repo::discover(std::path::Path::new("."))?;
let cfg = copytree::config::Config::load(&repo.main)?;
let opts = copytree::workspace::NewOptions { name: Some("task-42".into()), ..Default::default() };
let ws = copytree::workspace::new(&repo, &cfg, &opts, &mut |m| eprintln!("{m}"))?;
println!("{}", ws.workspace.path.display());
```

## Building and testing

```console
cargo build --release          # target/release/copytree and git-copytree
cargo test                     # unit tests
tests/e2e.sh /some/dir         # real npm, pnpm, bun and uv repos, hook mode, rollback
```

Run the e2e script once on a copy-on-write volume and once on ext4 to cover both clone paths. On Linux you can make a reflink-capable XFS volume with `truncate -s 3G x.img && mkfs.xfs -m reflink=1 x.img && mount -o loop x.img /mnt/x`.

## Status and roadmap

v0.1 covers `new`, `list`, `rm`, hook mode, deps, seeding and carry. Planned next, following the design doc:

- **v0.2:** a recycled workspace pool and hot spares, `gc` with TTLs, unique-bytes accounting, the APFS/btrfs whole-tree source clone fast path.
- **v0.3:** a pre-commit secret guard for carried values, `land` and `apply`.
- **v0.4:** overlayfs base layers on ext4, `dedupe`, an MCP server.

Tested on Linux (ext4, and XFS with reflink) and macOS (APFS). Windows compiles but hasn't been run yet.
