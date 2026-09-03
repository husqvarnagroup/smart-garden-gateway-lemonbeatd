// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

use anyhow::Context as _;

/// Load MSC from a hardcoded storage-path.
///
/// This can handle the case where the storage is partially corrupted since
/// it doesn't attempt to load any value-descriptions or other values.
pub async fn load_msc_hacky<P: AsRef<std::path::Path>>(devdir: P) -> Result<u32, crate::Error> {
    let devdir = devdir.as_ref().to_path_buf();
    let res: Result<_, crate::Error> = tokio::task::spawn_blocking(move || {
        crate::storage::Value::load(
            devdir.join("Value/Value_1r.json"),
            &[crate::storage::ValueDescription {
                id: 1,
                permission: lsdl::Permission::ReadWrite,
                name: None,
                type_id: lsdl::ValueType::Counter,
                format: crate::storage::ValueFormat::Binary { max_length: 4 },
                persistent: false,
                virtual_value: false,
            }],
        )
        .context("can't load MSC value directly")
    })
    .await
    .context("can't spawn task for loading MSC from storage")?;

    let value = res?;

    let msc = u32::from_be_bytes(
        value
            .as_binary()?
            .try_into()
            .context("MAC sequence count has wrong size for u32")?,
    );

    tracing::info!("Successfully loaded MSC `{}` directly", msc);

    Ok(msc)
}
