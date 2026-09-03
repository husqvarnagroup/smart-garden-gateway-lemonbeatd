#!/usr/bin/env bash

# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu -o pipefail

function help() {
  echo "Usage: $0 <version>"
}

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  help
  exit 1
fi

if [[ $(git status --porcelain) ]]; then
  echo "git repo contains local changes - stopping."
  exit 1
fi

readonly version=$1
previous_version=$(git describe --tags --abbrev=0 | sed 's/^v//')
readonly previous_version

readonly cargo_toml="Cargo.toml"

readonly source_branch="${SOURCE_BRANCH:-main}"
readonly release_pr_branch="release-prepare-$version"

git fetch
git checkout "$source_branch"
git pull

git checkout -B "$release_pr_branch"

sed -i "0,/version =/{s/version = \".*\"/version = \"$version\"/}" "${cargo_toml}"
cargo build

git add "${cargo_toml}" "Cargo.lock"
git commit -m "lemonbeatd: Bump version to $version"
git push --set-upstream origin "$release_pr_branch" --force

echo "*********************"
echo "Once this PR got merged, run the following command in the smart-garden-gateway repo:"
echo
echo -n "scripts/rust-recipe.sh -c $(readlink -f "$(dirname "$0")")/lemonbeatd "
echo "-r yocto/meta-gardena/recipes-connectivity/lemonbeatd/lemonbeatd_$previous_version.bb"
echo
echo "*********************"

git tag "v$version"
git push --tag
