<!--
SPDX-FileCopyrightText: GARDENA GmbH

SPDX-License-Identifier: GPL-3.0-or-later
-->

# Lemonbeat Server

## Prerequisites

1. Install [rustup](https://rustup.rs). This will then automatically install the required toolchain.
2. Initialize and update git submodules

    ```bash
    git submodule update --init
    ```

3. Install dependencies

    ```bash
    sudo apt install libdbus-1-dev libgirepository1.0-dev cmake ntp libcairo2-dev
    ```

4. Make sure that ntpd is running and listening on the ppp0 interface if devices
   should sync their clocks
5. [BNW Lemonbeat Dongle](https://confluence-husqvarna.riada.se/x/VSBNDQ). Best to
   [set up udev rules](https://confluence-husqvarna.riada.se/x/VyBNDQ) to have
   `/dev/bnw-gateway-dongle-ppp` automatically created when dongle is connected.
   Be sure to define a high priority in the rules name, like `/etc/udev/rules.d/99-local.rules`

## Run Server

### Setup radio module

1. (Re-) connect BNW Lemonbeat Dongle
2. Start pppd
   ```bash
   sudo ./start-pppd.sh
   ```
3. Configure ppp interface
   ```bash
   sudo ./configure-ppp.sh ppp0
   ```

### Run lemonbeat server

Use the following command to start the server. Note: at first launch this will create
a ```work``` directory in the current directory. Use ```--work-dir``` to specify a
custom directory.

```bash
RUST_LOG=info cargo run
```

If you are running into build errors because pkg-config can't find `dbus-1`, try to set PKG_CONFIG_PATH, e.g:

```bash
PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig/ RUST_LOG=info cargo run
```

## Run Tests

```bash
cargo test
```

## Run Component Tests

See [tests/README.md](tests/README.md).

## Documentation

Use the following command to generate the documentation.

```bash
cargo doc --document-private-items
```

Open the file ```target/doc/lemonbeatd/index.html``` as a starting point.

## Run Super-Linter Locally

```sudo docker run -e RUN_LOCAL=true -e VALIDATE_CLANG_FORMAT=false -e VALIDATE_CPP=false -e VALIDATE_GITHUB_ACTIONS=false -e VALIDATE_JSCPD=false -e VALIDATE_PYTHON_ISORT=false -e FILTER_REGEX_EXCLUDE=.*test/.*.json -v $PWD:/tmp/lint github/super-linter:slim-v4```

### Prepare release

1. Make sure to not have any local changes in the repository

2. Run release script passing it the new version and create PR
   ```bash
   ./release.sh x.y.1
   ```

3. When PR is merged, make sure a Git tag was created as expected (on main branch).
   In the `smart-garden-gateway` repository, run:
   ```txt
   scripts/rust-recipe.sh -c ../sg-lemonbeat-cargo -r yocto/meta-gardena/recipes-connectivity/lemonbeatd_x.y.0.bb
   ```
