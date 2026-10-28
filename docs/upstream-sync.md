# Syncing with the original SoundCraft project

LiVi Wave started as a rebrand of [SoundCraft](https://github.com/storytold/soundcraft). To take in
its newer commits, merge them and re-apply the rename:

```sh
scripts/sync-upstream.sh          # fetch, merge on sync/upstream-<date>, rename, cargo fmt; stops before committing
cargo xtask ci                    # the full gate (from a Visual Studio developer prompt on Windows)
git commit                        # finish the merge
git checkout main && git merge --ff-only sync/upstream-<date>
```

The script needs a git identity (`git config user.name/user.email`, or `SYNC_GIT_NAME` and
`SYNC_GIT_EMAIL`) and a remote called `upstream`, which it adds when missing.

## What it does

1. **Merges** `upstream/main` into a new branch, without committing.
2. **Keeps deleted** what we removed on purpose (`docs/brand/`, Dependabot, the CodeRabbit config).
3. **Merges renamed files three-way.** Upstream's lines say `soundcraft` where ours say `liviwave`, so a plain
   merge conflicts everywhere. The script applies `scripts/rename-rules.pl` to the merge base and to upstream's
   version first, then merges those with ours, so only real differences conflict.
4. **Takes in new upstream files** under their new names, and drops files upstream deleted.
5. **Re-applies the rename** to everything the merge changed, runs `cargo fmt` (it re-sorts `use` lines, which
   is the usual cause of leftover conflicts) and lets cargo rewrite `Cargo.lock`.

It refuses to finish while conflict markers remain.

## When it stops with conflicts

Open each listed file. Import blocks almost always differ only in sort order: upstream's side is the newer
code, so `scripts/sync-upstream.sh --theirs FILE...` takes it. Afterwards run `cargo check --workspace --all-targets`: taking one side of an import block can lose a `use` line that sat just outside the hunk, or leave one twice. Anything else, merge by hand and `git add`
the file. Then `scripts/sync-upstream.sh --continue`.

## After a sync, look at

- **New workflows** under `.github/`: they would run on GitHub-hosted runners. CI here runs on our own runner
  (`docs/self-hosted-runner.md`), so delete or disable them.
- **ArtCraft, Discord or getartcraft** in the added lines: the report lists them; remove or rewrite.
- **`craft_update::suite::SOUNDCRAFT`**: an identifier in `craft-libs`, so the rename leaves it alone. Our own
  update entry is `LIVIWAVE` in `crates/ui-egui/src/updates_ui.rs`.
- **Identifiers from other projects** (`craft-ui`, `craft-docpath`, ...) must keep their names. If the rename
  ever breaks one, add it to the protected names at the top of `scripts/rename-rules.pl`.

Keep our own features in new files and modules where you can: the fewer upstream files we edit, the fewer
conflicts each sync has.
