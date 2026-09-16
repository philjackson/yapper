#!/usr/bin/env bash
# Cut a release: bump the version, tag it, and let the workflow build it.
#
#   ./scripts/release.sh 0.1.3     an explicit version
#   ./scripts/release.sh patch     bump the last number of the current one
#   ./scripts/release.sh minor
#   ./scripts/release.sh major
#
# Everything local happens first and is shown to you; nothing is pushed until
# you say so. -y skips the question.
set -euo pipefail

cd "$(dirname "$0")/.."

yes_please=false
wanted=""
for argument in "$@"; do
    case $argument in
        -y|--yes) yes_please=true ;;
        -*) echo "unknown option: $argument" >&2; exit 2 ;;
        *) wanted=$argument ;;
    esac
done

die() { echo "release: $*" >&2; exit 1; }

current=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[[ -n $current ]] || die "cannot read the current version from Cargo.toml"

case $wanted in
    major|minor|patch)
        IFS=. read -r major minor patch <<< "$current"
        case $wanted in
            major) version="$((major + 1)).0.0" ;;
            minor) version="${major}.$((minor + 1)).0" ;;
            patch) version="${major}.${minor}.$((patch + 1))" ;;
        esac
        ;;
    "")  die "usage: release.sh <version|major|minor|patch> [-y]" ;;
    *)   version=$wanted ;;
esac

[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "'$version' is not a X.Y.Z version"
tag="v$version"

# --- everything that would make a mess if it were wrong ---------------------

branch=$(git rev-parse --abbrev-ref HEAD)
[[ $branch == main ]] || die "on branch '$branch', expected main"
[[ -z $(git status --porcelain) ]] || die "working tree is dirty; commit or stash first"

git fetch --quiet origin
[[ $(git rev-parse HEAD) == $(git rev-parse origin/main) ]] ||
    die "main and origin/main disagree; push or pull first"

git rev-parse -q --verify "refs/tags/$tag" >/dev/null &&
    die "$tag already exists locally"
git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1 &&
    die "$tag already exists on the remote"
# A release without its tag still blocks the workflow, which uses release create.
if command -v gh >/dev/null && gh release view "$tag" >/dev/null 2>&1; then
    die "a release for $tag already exists; delete it or pick another version"
fi

# --- local work -------------------------------------------------------------

echo "release: $current -> $version"
sed -i "0,/^version = \"$current\"/s//version = \"$version\"/" Cargo.toml

echo "release: building"
cargo build --release >/dev/null
echo "release: testing"
cargo test --release >/dev/null

reported=$(./target/release/yapper --version | awk '{print $2}')
[[ $reported == "$version" ]] || die "binary reports $reported, expected $version"

git add Cargo.toml Cargo.lock
git commit --quiet --message "Version $version"
git tag --annotate "$tag" --message "yapper $version"

echo
echo "  commit: $(git log --oneline -1)"
echo "  tag:    $tag"
echo "  pushes: main and $tag to origin"
echo

if ! $yes_please; then
    read -r -p "Push and release? [y/N] " reply
    case $reply in
        [yY]*) ;;
        *)
            git tag --delete "$tag" >/dev/null
            git reset --quiet --hard HEAD~1
            echo "release: undone, nothing pushed"
            exit 1
            ;;
    esac
fi

git push --quiet origin main
git push --quiet origin "$tag"
echo "release: pushed. The workflow builds and publishes it:"
echo "  https://github.com/philjackson/yapper/actions"
