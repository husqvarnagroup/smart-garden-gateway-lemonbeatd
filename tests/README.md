<!--
SPDX-FileCopyrightText: GARDENA GmbH

SPDX-License-Identifier: GPL-3.0-or-later
-->

# lemonbeatd-tests

## Install Dependencies

For Ubuntu:

```bash
apt install bubblewrap libgirepository-1.0-dev inotify-tools
```

## AppArmor Rule

From Ubuntu 24.04 onwards a rule for AppArmor to allow `bwrap` is needed.

Add the file `/etc/apparmor.d/bwrap` with the content:

```text
abi <abi/4.0>,
include <tunables/global>

profile bwrap /usr/bin/bwrap flags=(unconfined) {
  userns,

  # Site-specific additions and overrides. See local/README for details.
  include if exists <local/bwrap>
}
```

Then run:

```bash
systemctl reload apparmor
```

## Current directory

For all documentation in this file it's assumed that you're in the directory
`tests`.

## Preparation

**Note:** Keep in mind that you need an internet connection for many of these
steps so they won't work in a network namespace.

### setup virtualenv

```bash
poetry install --no-root
```

## Usage (`run_ci`)

You can use the pytest wrapper with the same arguments as pytest itself.
All it does is setup an environment for pytest to test in.

Run all tests:
```bash
./scripts/pytest
```

You also can open a shell inside the test environment to reduce startup times.
Inside, you can use `poetry run pytest` as usual.
```bash
SHELL_ONLY=1 ./scripts/pytest
```
