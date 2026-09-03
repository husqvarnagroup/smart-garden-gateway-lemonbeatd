// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), lemonbeatd::Error> {
    #[cfg(not(feature = "tokio_tracing"))]
    gardenalog::init_tracing();

    let mut config = lemonbeatd::Config::load("config.yml").await?;
    if config.is_none() {
        config = lemonbeatd::Config::load("/etc/lemonbeatd.yml").await?;
    }

    if let Some(config) = config {
        lemonbeatd::set_config(config);
    }

    let res = lemonbeatd::run().await;
    if let Err(e) = &res {
        tracing::error!("{:?}", e);
        std::process::exit(1);
    }

    Ok(())
}
