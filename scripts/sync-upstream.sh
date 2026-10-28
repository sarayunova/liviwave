#!/usr/bin/env bash
# Merge the original SoundCraft project (remote `upstream`) into LiVi Wave and re-apply the rename.
#
#   scripts/sync-upstream.sh              fetch, merge on a new branch, rename, format; stops before committing
#   scripts/sync-upstream.sh --continue   after you resolved merge conflicts by hand: rename and format
#
# Needs a git identity: `git config user.name/user.email`, or SYNC_GIT_NAME and SYNC_GIT_EMAIL.
# Then review `git status`, run `cargo xtask ci`, commit, fast-forward main. See docs/upstream-sync.md.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

UP="${UPSTREAM_REF:-upstream/main}"
NAME="${SYNC_GIT_NAME:-$(git config user.name || true)}"
EMAIL="${SYNC_GIT_EMAIL:-$(git config user.email || true)}"
[[ -n "$NAME" && -n "$EMAIL" ]] || { echo "error: set SYNC_GIT_NAME and SYNC_GIT_EMAIL (or git config user.name/user.email)" >&2; exit 1; }
G=(git -c "user.name=$NAME" -c "user.email=$EMAIL")

# Things we removed on purpose; if upstream changes them, the merge keeps them deleted.
KEEP_DELETED='^(docs/brand/|\.github/dependabot\.yml$|\.coderabbit\.yaml$)'
# Files where the Windows suite folder "ArtCraft" becomes "LiVi".
SUITE_FILES='^(packaging/windows/|docs/releasing\.md$|\.github/workflows/windows-arm64\.yml$)'

rename_pass() {
  local f
  git diff HEAD --name-only --diff-filter=AMR | while IFS= read -r f; do
    [[ -f "$f" ]] || continue
    case "$f" in *.png|*.ico|*.icns|*.tiff|*.DS_Store|contributors/contributors.json|Cargo.lock|ATTRIBUTION.md) continue ;; esac
    perl -i -p scripts/rename-rules.pl "$f"
    if [[ "$f" =~ $SUITE_FILES ]]; then perl -i -pe 's/ArtCraft/LiVi/g' "$f"; fi
  done
  # New upstream files whose names carry the old one.
  git ls-files | grep -i soundcraft | while IFS= read -r f; do
    n="$(dirname "$f")/$(basename "$f" | sed 's/soundcraft/liviwave/g;s/SoundCraft/LiViWave/g')"
    mkdir -p "$(dirname "$n")"; git mv "$f" "$n"
  done || true
  # Cargo.lock: let cargo rewrite the package names.
  cargo metadata --format-version 1 >/dev/null 2>&1 || true
  cargo fmt
  git add -A
}

report() {
  echo; echo "== review =="
  echo "-- new or changed workflows (they would run on GitHub-hosted runners; we run CI on our own):"
  git diff --cached --name-only HEAD -- .github | sed 's/^/   /'
  echo "-- ArtCraft / Discord / getartcraft in the merged change:"
  git diff --cached -U0 HEAD | grep -E '^\+' | grep -iE 'getartcraft|discord\.gg|ArtCraft' | grep -v 'based on' | head -10 | sed 's/^/   /' || true
  echo "-- suite SOUNDCRAFT use (our update entry is LIVIWAVE):"
  git grep -n 'suite::SOUNDCRAFT' -- '*.rs' | sed 's/^/   /' || true
  echo; git diff --cached --stat HEAD | tail -1
  echo "next: cargo xtask ci, then commit, then: git checkout main && git merge --ff-only $(git branch --show-current)"
}

if [[ "${1:-}" == "--continue" ]]; then
  if git ls-files -u | grep -q .; then echo "error: unresolved conflicts remain:" >&2; git ls-files -u | cut -f2 | sort -u >&2; exit 1; fi
  rename_pass; report; exit 0
fi

git diff --quiet && git diff --cached --quiet || { echo "error: working tree not clean" >&2; exit 1; }
git remote get-url upstream >/dev/null 2>&1 || git remote add upstream https://github.com/storytold/soundcraft.git
git fetch -q upstream
ahead="$(git rev-list --count "HEAD..$UP")"
[[ "$ahead" != 0 ]] || { echo "up to date with $UP"; exit 0; }
echo "$ahead upstream commit(s) to merge:"; git log --oneline "HEAD..$UP" | head -20

BR="sync/upstream-$(date +%Y%m%d)"
git checkout -q -b "$BR"
"${G[@]}" merge --no-ff --no-commit "$UP" >/dev/null 2>&1 || true

# Conflicts we know how to settle: deletions we made on purpose, and renamed-away files.
git status --porcelain | grep -E '^(DU|UD|AU|UA) ' | cut -c4- | while IFS= read -r f; do
  if [[ "$f" =~ $KEEP_DELETED ]]; then git rm -q -f -- "$f"; echo "kept deleted: $f"
  elif [[ "$f" == *soundcraft* ]]; then
    n="$(dirname "$f")/$(basename "$f" | sed 's/soundcraft/liviwave/g')"
    git show "$UP:$f" > "$n" && git add -- "$n" && git rm -q -f -- "$f"; echo "moved to new name: $f -> $n"
  fi
done

# Rename-aware three-way merge. Every upstream line says `soundcraft` where ours says `liviwave`, so
# git sees conflicts that are not real. Apply the rename to the merge base and to upstream's version,
# then merge those with ours: only genuine differences are left to conflict.
base="$(git merge-base HEAD "$UP")"
renamed() {  # renamed REF OLDPATH NEWPATH
  git show "$1:$2" | perl -p scripts/rename-rules.pl | { if [[ "$3" =~ $SUITE_FILES ]]; then perl -pe 's/ArtCraft/LiVi/g'; else cat; fi; }
}
git ls-files -u | cut -f2 | sort -u | while IFS= read -r f; do
  old="$(echo "$f" | sed 's/liviwave/soundcraft/g;s/LiViWave/SoundCraft/g')"
  if git cat-file -e "$base:$old" 2>/dev/null && git cat-file -e "$UP:$old" 2>/dev/null && git cat-file -e "HEAD:$f" 2>/dev/null; then
    tmp="$(mktemp -d)"
    git show "HEAD:$f" > "$tmp/ours"
    renamed "$base" "$old" "$f" > "$tmp/base"
    renamed "$UP" "$old" "$f" > "$tmp/theirs"
    if git merge-file -p "$tmp/ours" "$tmp/base" "$tmp/theirs" > "$tmp/out"; then
      cp "$tmp/out" "$f"; git add -- "$f"; echo "auto-merged: $f"
    else
      cp "$tmp/out" "$f"; echo "needs a human (conflict markers in the file): $f"
    fi
    rm -rf "$tmp"
  fi
done

if git ls-files -u | grep -q .; then
  echo; echo "still to resolve by hand (look for <<<<<<< markers), then git add each file:"
  git ls-files -u | cut -f2 | sort -u | sed 's/^/   /'
  echo "then run: scripts/sync-upstream.sh --continue"; exit 2
fi
rename_pass; report
