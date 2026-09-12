#!/usr/bin/env bash
set -euo pipefail

version=$(python3 deploy/ci/check-version.py)
tag=v$version
release_sha=${RELEASE_SHA:?RELEASE_SHA is required}
current_main_sha=${CURRENT_MAIN_SHA:?CURRENT_MAIN_SHA is required}
should_release=false
if [[ "$release_sha" != "$(git rev-parse HEAD)" ]]; then
    echo 'Checkout does not match the successful CI commit' >&2
    exit 1
fi
if [[ "$release_sha" != "$current_main_sha" ]]; then
    echo 'Skipping superseded main commit' >&2
elif git rev-parse --verify "refs/tags/$tag" >/dev/null 2>&1; then
    tagged_sha=$(git rev-parse "refs/tags/$tag^{commit}")
    if [[ "$tagged_sha" != "$release_sha" ]]; then
        version_commits=$(git rev-list "$tagged_sha..$release_sha" -- VERSION)
        if ! git merge-base --is-ancestor "$tagged_sha" "$release_sha" || [[ -n "$version_commits" ]]; then
            echo "Tag $tag already belongs to another commit; choose a new version" >&2
            exit 1
        fi
    fi
    echo "Skipping existing tag $tag; published versions are immutable" >&2
else
    should_release=true
fi
printf 'version=%s\ntag=%s\nshould_release=%s\n' "$version" "$tag" "$should_release" >> "${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"
