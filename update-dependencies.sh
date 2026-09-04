#!/usr/bin/env bash

# SPDX-FileCopyrightText: GARDENA GmbH
#
# SPDX-License-Identifier: GPL-3.0-or-later

set -eu -o pipefail

if [[ $(git status --porcelain) ]]; then
  echo "git repo contains local changes - stopping."
  exit 1
fi

branch="cargo-dependency-update-$(date +"%Y-%m-%d")"

git checkout main
git pull
git checkout -B "$branch"

cargo update

# check that project still compiles with the Rust version from the gateway build
rustup run cargo build --all

git add Cargo.lock

git commit -m "chore: update cargo dependencies"
git push --set-upstream origin "$branch" --force
