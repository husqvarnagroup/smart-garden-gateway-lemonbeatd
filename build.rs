// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::Write as _;

trait OutputEx {
    fn is_ok(&self) -> anyhow::Result<()>;
    fn stdout(self) -> anyhow::Result<Vec<u8>>;
}

impl OutputEx for std::process::Output {
    fn is_ok(&self) -> anyhow::Result<()> {
        if !self.status.success() {
            anyhow::bail!(
                "failed with: {:?}: \n{}",
                self.status.code(),
                std::str::from_utf8(&self.stderr).unwrap_or(&hex::encode(&self.stderr))
            );
        }
        Ok(())
    }

    fn stdout(self) -> anyhow::Result<Vec<u8>> {
        self.is_ok()?;
        Ok(self.stdout)
    }
}

fn main() {
    let head = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .unwrap()
        .stdout()
        .unwrap();
    let head = std::str::from_utf8(&head).unwrap();
    let head_short = &head[..12];

    let pkgver = std::env::var("CARGO_PKG_VERSION").unwrap();

    let dirty = !std::process::Command::new("git")
        .args(["--no-optional-locks", "status", "-uno", "porcelain"])
        .output()
        .unwrap()
        .stdout()
        .unwrap()
        .is_empty()
        || !std::process::Command::new("git")
            .args(["diff-index", "--name-only", "HEAD"])
            .output()
            .unwrap()
            .stdout()
            .unwrap()
            .is_empty();

    let out_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut f = std::fs::File::create(out_path.join("version.rs")).unwrap();
    writeln!(
        &mut f,
        "const VERSION: &str = \"{}-g{}{}\";",
        pkgver,
        head_short,
        if dirty { "-dirty" } else { "" }
    )
    .unwrap();
}
