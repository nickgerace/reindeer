/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use globset::GlobBuilder;
use globset::GlobSetBuilder;
use ignore::gitignore::GitignoreBuilder;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;

use crate::buckify::relative_path;
use crate::cargo;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::Args;
use crate::Paths;

#[derive(Debug, Deserialize, Serialize)]
struct CargoChecksums {
    files: IndexMap<String, String>,
    package: Option<String>,
}

pub(crate) fn cargo_vendor(
    config: &Config,
    no_delete: bool,
    audit_sec: bool,
    no_fetch: bool,
    args: &Args,
    paths: &Paths,
) -> Result<()> {
    let vendordir = Path::new("vendor"); // relative to third_party_dir

    let mut cmdline = vec![
        "vendor",
        "--manifest-path",
        paths.manifest_path.to_str().unwrap(),
        vendordir.to_str().unwrap(),
        "--versioned-dirs",
    ];
    if no_delete {
        cmdline.push("--no-delete");
    }

    let configdir = paths.third_party_dir.join(".cargo");
    fs::create_dir_all(&configdir)?;

    log::info!("Running cargo {:?}", cmdline);
    let cargoconfig = cargo::run_cargo(
        config,
        Some(&paths.cargo_home),
        &paths.third_party_dir,
        args,
        &cmdline,
    )?;

    fs::write(configdir.join("config"), cargoconfig)?;

    if let Some(vendor_config) = &config.vendor {
        filter_checksum_files(&paths.third_party_dir, vendordir, vendor_config)?;
    }

    if audit_sec {
        crate::audit_sec::audit_sec(config, paths, no_fetch, false).context("doing audit_sec")?;
    }

    Ok(())
}

fn filter_checksum_files(
    third_party_dir: &Path,
    vendordir: &Path,
    config: &VendorConfig,
) -> Result<()> {
    if config.checksum_exclude.is_empty() && config.gitignore_checksum_exclude.is_empty() {
        return Ok(());
    }

    log::debug!(
        "vendor.gitignore_checksum_exclude = {:?} vendor.checksum_exclude = {:?}",
        config.gitignore_checksum_exclude,
        config.checksum_exclude
    );

    // re-write checksum files to exclude things we don't want (like Cargo.lock)
    let mut remove_globs = GlobSetBuilder::new();
    for glob in &config.checksum_exclude {
        let glob = GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
            .with_context(|| format!("Invalid checksum exclude glob `{}`", glob))?;
        remove_globs.add(glob);
    }
    let remove_globs = remove_globs.build()?;

    let mut gitignore = GitignoreBuilder::new(third_party_dir);
    for ignore in &config.gitignore_checksum_exclude {
        if let Some(err) = gitignore.add(third_party_dir.join(ignore)) {
            log::warn!(
                "Failed to read ignore file {}: {}; skipping",
                ignore.display(),
                err
            );
        }
    }
    let gitignore = gitignore.build()?;

    log::debug!(
        "remove_globs {:#?}, gitignore {:#?}",
        remove_globs,
        gitignore
    );

    for entry in fs::read_dir(third_party_dir.join(vendordir))? {
        let entry = entry?;
        let path = entry.path(); // full/path/to/vendor/foo-1.2.3
        let checksum = path.join(".cargo-checksum.json"); // full/path/to/vendor/foo-1.2.3/.cargo-checksum.json

        log::trace!("Reading checksum {}", checksum.display());

        let file = match fs::read(&checksum) {
            Err(err) => {
                log::warn!("Failed to read {}: {}", checksum.display(), err);
                continue;
            }
            Ok(file) => file,
        };

        let mut checksums: CargoChecksums = match serde_json::from_slice(&file) {
            Err(err) => {
                log::warn!("Failed to deserialize {}: {}", checksum.display(), err);
                continue;
            }
            Ok(cs) => cs,
        };

        let mut changed = false;

        let pkgdir = relative_path(third_party_dir, &path); // vendor/foo-1.2.3

        checksums.files.retain(|k, _| {
            log::trace!("{}: checking {}", checksum.display(), k);
            let del = remove_globs.is_match(k)
                || gitignore
                    .matched_path_or_any_parents(pkgdir.join(k), false)
                    .is_ignore();
            if del {
                log::debug!("{}: removing {}", checksum.display(), k);
                changed = true;
            };
            !del
        });

        if changed {
            log::info!("Rewriting checksum {}", checksum.display());
            fs::write(checksum, serde_json::to_vec(&checksums)?)?;
        }
    }

    Ok(())
}
