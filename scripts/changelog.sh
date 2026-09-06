#!/usr/bin/env bash
#
# changelog.sh — release notes for a tag.
#
# `changelog/<tag>.md` in the repo is the **source of truth**. GitHub Releases
# and luvus.dev/changelog may enrich its contributor links with avatars, while
# the notes themselves stay version-controlled and reviewable in one place.
#
#   scripts/changelog.sh v0.8.1            # print the notes (curated file if it
#                                          # exists, else generated from commits)
#   scripts/changelog.sh v0.8.1 --write    # create changelog/v0.8.1.md if absent
#   scripts/changelog.sh v0.8.1 --write --force   # regenerate, discarding edits
#   scripts/changelog.sh v0.8.1 v0.8.0     # explicit base tag
#
# Printing strips the YAML front matter, since a GitHub Release body has no use
# for it — the website reads it for the version/date.
set -euo pipefail

REPO="${GITHUB_REPOSITORY:-RizRiyz/luvus}"
NEW=""
PREV=""
WRITE=0
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --write) WRITE=1 ;;
    --force) FORCE=1 ;;
    -*) printf 'unknown flag: %s\n' "$arg" >&2; exit 2 ;;
    *) if [ -z "$NEW" ]; then NEW="$arg"; else PREV="$arg"; fi ;;
  esac
done
[ -n "$NEW" ] || { echo "usage: changelog.sh <new-tag> [prev-tag] [--write] [--force]" >&2; exit 2; }

ROOT="$(git rev-parse --show-toplevel)"
FILE="$ROOT/changelog/$NEW.md"

# Strip YAML front matter, then any leading blank lines.
strip_front_matter() {
  awk 'NR==1 && $0=="---" {fm=1; next} fm && $0=="---" {fm=0; next} !fm' \
    | awk 'NF {p=1} p'
}

# GitHub Releases can render profile pictures even though the source Markdown
# stays portable for the terminal app. Add an avatar row above curated linked
# contributor and issue-reporter lists, then retain each text list as the
# accessible fallback.
render_release_notes() {
  strip_front_matter | perl -0pe '
    s{(^## (?:Contributors|Issue reporters)[ \t]*\r?\n(?:\r?\n)?)((?:^[ \t]*-[^\r\n]*(?:\r?\n|$))+)}{
      my ($heading, $list) = ($1, $2);
      my @avatars;
      while ($list =~ m{^[ \t]*- \[([^\]]+)\]\(https://github\.com/([A-Za-z0-9-]+)/?\)[ \t]*$}gm) {
        my ($label, $login) = ($1, $2);
        $label =~ s/&/&amp;/g;
        $label =~ s/"/&quot;/g;
        $label =~ s/</&lt;/g;
        $label =~ s/>/&gt;/g;
        push @avatars, qq{<a href="https://github.com/$login" title="$label"><img src="https://github.com/$login.png?size=80" alt="$label" width="40" height="40"></a>};
      }
      @avatars
        ? $heading . "<p>\n" . join("\n", @avatars) . "\n</p>\n\n" . $list
        : $heading . $list;
    }egm;
  '
}

# Print an existing curated file without regenerating over hand-written notes.
if [ "$WRITE" = 0 ] && [ -f "$FILE" ]; then
  render_release_notes < "$FILE"
  exit 0
fi
if [ "$WRITE" = 1 ] && [ -f "$FILE" ] && [ "$FORCE" = 0 ]; then
  printf 'changelog/%s.md already exists — edit it, or pass --force to regenerate.\n' "$NEW" >&2
  exit 0
fi

# Previous version tag: newest strict vX.Y.Z that isn't NEW.
[ -n "$PREV" ] || PREV="$(git tag --list 'v[0-9]*.[0-9]*.[0-9]*' --sort=-version:refname \
                          | grep -vxF "$NEW" | head -n1 || true)"

# Range end: the tag if it exists, else HEAD (so a pre-tag preview works).
END="$NEW"
git rev-parse -q --verify "${NEW}^{commit}" >/dev/null 2>&1 || END="HEAD"
RANGE="${PREV:+$PREV..}$END"

KNOWN='feat|fix|change|refactor|perf|style|chore|ci|build|docs|test'
# Release plumbing and dependency churn are not user-facing news.
NOISE='^(release|homebrew|bump|bum|merge)\b|^chore\(deps'

# Turn a commit subject into a readable bullet: drop the Conventional-Commit
# type, keep any scope as a lead-in, and capitalize the first letter.
polish() {
  local subj="$1" scope body
  scope="$(printf '%s' "$subj" | sed -nE 's/^[a-zA-Z]+\(([^)]+)\)!?:.*/\1/p')"
  body="$(printf '%s' "$subj" | sed -E 's/^[a-zA-Z]+(\([^)]*\))?!?:[[:space:]]*//')"
  # A squash-merge PR suffix is rendered as a link beside the title, not left
  # inside the title as plain text.
  body="$(printf '%s' "$body" | sed -E 's/[[:space:]]+\(#[0-9]+\)$//')"
  # Uppercase the first letter portably: sed's \U is GNU-only (macOS ships BSD
  # sed) and ${var^} needs bash 4 (macOS ships 3.2).
  body="$(printf '%s' "${body%"${body#?}"}" | tr '[:lower:]' '[:upper:]')${body#?}"
  if [ -n "$scope" ]; then
    scope="$(printf '%s' "${scope%"${scope#?}"}" | tr '[:lower:]' '[:upper:]')${scope#?}"
    printf '**%s:** %s' "$scope" "$body"
  else
    printf '%s' "$body"
  fi
}

# References follow the description so a reader sees the behavior before metadata.
# Squash commits carry `(#N)` in their subject; direct commits have no PR link.
refs() {
  local subj="$1" hash="$2" pr
  pr="$(printf '%s' "$subj" | sed -nE 's/.* \(#([0-9]+)\)$/\1/p')"
  if [ -n "$pr" ]; then
    printf '[`%s`](https://github.com/%s/commit/%s), [#%s](https://github.com/%s/pull/%s)' \
      "$hash" "$REPO" "$hash" "$pr" "$REPO" "$pr"
  else
    printf '[`%s`](https://github.com/%s/commit/%s)' "$hash" "$REPO" "$hash"
  fi
}

# $1 = heading   $2 = ERE of commit types to include
section() {
  local out="" subj hash
  while IFS=$'\t' read -r subj hash; do
    [ -n "$subj" ] || continue
    printf '%s' "$subj" | grep -qiE "^($2)(\(.+\))?!?:" || continue
    printf '%s' "$subj" | grep -qiE "$NOISE" && continue
    out+="- $(polish "$subj") ($(refs "$subj" "$hash"))."$'\n'
  done < <(git log "$RANGE" --no-merges --pretty=tformat:'%s%x09%h')
  [ -n "$out" ] && printf '## %s\n\n%s\n' "$1" "$out"
  return 0
}

other() {
  local out="" subj hash
  while IFS=$'\t' read -r subj hash; do
    [ -n "$subj" ] || continue
    printf '%s' "$subj" | grep -qiE "^($KNOWN)(\(.+\))?!?:" && continue
    printf '%s' "$subj" | grep -qiE "$NOISE" && continue
    out+="- $(polish "$subj") ($(refs "$subj" "$hash"))."$'\n'
  done < <(git log "$RANGE" --no-merges --pretty=tformat:'%s%x09%h')
  [ -n "$out" ] && printf '## Other\n\n%s\n' "$out"
  return 0
}

notes() {
  # Front matter — the website reads version + date from here; printing strips it.
  printf -- '---\nversion: %s\ndate: %s\n---\n\n' "$NEW" "$(date -u +%Y-%m-%d)"

  # A prose lead. Generated notes can only ever restate commit subjects, so leave
  # an explicit prompt: these notes exist to *explain the work*, not to list it.
  # Delete the blockquote once written.
  #
  # NOTE the hard-wrap warning below — GitHub renders release notes with GFM hard
  # line breaks, so a newline inside a paragraph shows up as a literal <br>.
  printf '> _Write 1-2 short sentences naming the headline changes and compatibility._\n'
  printf '>\n'
  printf '> _Keep each bullet to one direct sentence, drop internal-only work, and delete this note._\n'
  printf '>\n'
  printf '> _Keep every paragraph and bullet on ONE line — do not hard-wrap. GitHub renders release notes with hard line breaks, so a wrapped paragraph shows a break after every line. Delete this note when done._\n\n'

  section 'Features' 'feat'
  section 'Improvements' 'change|refactor|perf|style|build|docs'
  section 'Fixes' 'fix'
  other

  # %aN applies .mailmap, so one person's several git names collapse to one.
  local authors
  authors="$(git log "$RANGE" --no-merges --pretty=tformat:'%aN' | sort -u | sed 's/^/- /')"
  [ -n "$authors" ] && printf '## Contributors\n\n%s\n\n' "$authors"

  printf '## Full changelog\n\n'
  if [ -n "$PREV" ]; then
    printf 'https://github.com/%s/compare/%s...%s\n' "$REPO" "$PREV" "$NEW"
  else
    printf 'https://github.com/%s/commits/%s\n' "$REPO" "$NEW"
  fi
}

if [ "$WRITE" = 1 ]; then
  mkdir -p "$ROOT/changelog"
  notes > "$FILE"
  printf 'wrote changelog/%s.md — edit it before releasing.\n' "$NEW" >&2
else
  # Same rendering as the curated path, including profile avatars when the
  # generated notes eventually contain linked GitHub credit sections.
  notes | render_release_notes
fi
