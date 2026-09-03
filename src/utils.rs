// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Another one of those famous util libs 😉.

use anyhow::Context as _;

/// Use a closure to render into a format string.
///
/// Use this if you need `if/else` for a single format argument to change the
/// output depending on that condition.
/// - You'll not have to render strings into local variables first
/// - The types don't have to match
/// - You don't have to implement Display in a highly specialized,  single-use way.
/// - It's more efficient than allocating a separate String first just to render and discard it.
// Source: https://github.com/rust-lang/rust/blob/26c9b0046f96403cdf959e4e1f874ec25f9dbf6f/src/librustdoc/html/format.rs#L1474
// License: see the linked repo
// PANIC: this function does panic if you try to call `fmt` twice.
pub(crate) fn display_fn(
    f: impl FnOnce(&mut std::fmt::Formatter<'_>) -> std::fmt::Result,
) -> impl std::fmt::Display {
    struct WithFormatter<F>(std::cell::Cell<Option<F>>);

    impl<F> std::fmt::Display for WithFormatter<F>
    where
        F: FnOnce(&mut std::fmt::Formatter<'_>) -> std::fmt::Result,
    {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            (self.0.take()).unwrap()(f)
        }
    }

    WithFormatter(std::cell::Cell::new(Some(f)))
}

/// Simplify creating [version_compare::Version].
pub(crate) fn create_version(
    major: i32,
    minor: i32,
    patch: i32,
) -> version_compare::Version<'static> {
    version_compare::Version::from_parts(
        "",
        vec![
            version_compare::Part::Number(major),
            version_compare::Part::Number(minor),
            version_compare::Part::Number(patch),
        ],
    )
}

/// Get the current, monotonic gateway timestamp.
pub(crate) fn gateway_timestamp() -> u64 {
    let now = std::time::SystemTime::now();
    now.duration_since(std::time::UNIX_EPOCH)
        .map_or_else(
            |e| Err(e).context("duration_since"),
            |d| d.as_millis().try_into().context("try_into"),
        )
        .unwrap_or_else(|e| {
            tracing::error!("Failed to calculate gateway timestamp {:?}", e);
            0u64
        })
}
