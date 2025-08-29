// SPDX-License-Identifier: GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL
// SPDX-FileCopyrightText: 2025 Harald Sitter <sitter@kde.org>

use clap::Parser;
use core::panic;
use libbtrfsutil::{CreateSnapshotOptions, CreateSubvolumeOptions, DeleteSubvolumeOptions};
use libc::{EINVAL, ENOENT};
use sha2::{Digest, Sha256};
use std::os::unix::fs::symlink;
use std::{
    cmp::max,
    error::Error,
    fs::{self, File},
    io,
    path::{self, Path, PathBuf},
    process::Command,
    time::SystemTime,
};
use walkdir::{DirEntry, WalkDir};

const HASH_KEY: &str = "trusted.etc.factory.sha256";
const LINK_KEY: &str = "trusted.etc.factory.symlink-target";
const LAST_UPDATE_KEY: &str = "trusted.etc.factory.last-update";
const FACTORY: &str = "/usr/share/factory/etc/"; // Trailing slash is important; we use this as input for strip_prefix

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long)]
    migrate: bool,
    #[arg(long)]
    replace: bool,
}

fn seed_subvolume(etc: &str) -> Result<(), Box<dyn Error>> {
    CreateSubvolumeOptions::new().create(etc).map_err(|e| {
        eprintln!("Failed to create subvolume: {}", e);
        Box::new(e) as Box<dyn Error>
    })?;

    Command::new("cp")
        .arg("--archive")
        .arg("--recursive")
        .arg("--verbose")
        .arg("/etc/.")
        .arg(etc)
        .status()
        .map_err(|e| {
            eprintln!("Failed to copy etc contents: {}", e);
            Box::new(e) as Box<dyn Error>
        })?
        .success()
        .then(|| ())
        .ok_or_else(|| {
            let e = io::Error::new(io::ErrorKind::Other, "cp exited with failure");
            eprintln!("Failed to copy etc contents: {}", e);
            Box::new(e) as Box<dyn Error>
        })?;

    for entry in WalkDir::new(etc)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.metadata().unwrap().is_symlink())
    {
        let target = fs::read_link(entry.path())?;
        if target.to_string_lossy().contains(FACTORY) {
            eprintln!(
                "Removing symlink to factory: {} -> {}",
                entry.path().display(),
                target.display()
            );
            fs::remove_file(entry.path()).map_err(|e| {
                eprintln!("Failed to remove symlink {}: {}", entry.path().display(), e);
                Box::new(e) as Box<dyn Error>
            })?;
        }
    }
    Ok(())
}

fn create_subvolume(etc: &str, migrate: &bool) -> Result<(), Box<dyn Error>> {
    DeleteSubvolumeOptions::new()
        .recursive(true)
        .delete(etc)
        .or_else(|_e| {
            fs::remove_dir_all(etc).ok();
            Ok::<(), Box<dyn Error>>(())
        })?;
    CreateSnapshotOptions::new()
        .readonly(false)
        .recursive(true)
        .create("/etc", etc)
        .or_else(|e| {
            if e.errno() == EINVAL {
                if !migrate {
                    eprintln!("etc is not a subvolume, use --migrate to migrate it.");
                    panic!("etc is not a subvolume, use --migrate to migrate it.");
                }
                seed_subvolume(etc)
            } else {
                eprintln!("Failed to create snapshot: {}", e);
                Err(Box::new(e) as Box<dyn Error>)
            }
        })?;

    Ok(())
}

fn process_symlink(path: &Path, etc_path: PathBuf, migrate: &bool) -> Result<(), Box<dyn Error>> {
    let target = fs::read_link(&path).map_err(|e| {
        eprintln!("Failed to read symlink {}: {}", path.display(), e);
        Box::new(e) as Box<dyn Error>
    })?;
    if target == PathBuf::from("/dev/null") {
        eprintln!("Skipping /dev/null symlink at {}", path.display());
        return Ok(());
    }

    println!("Symlink target: {}", target.display());

    if target == fs::read_link(&etc_path).unwrap_or_default() {
        println!("Symlink unchanged: {}", etc_path.display());
        // Link is already correct.
        if *migrate {
            // But adopt it in case we are migrating
            xattr::set(
                etc_path,
                LINK_KEY,
                target.to_str().unwrap_or_default().as_bytes(),
            )?;
        }
        return Ok(());
    }

    match xattr::get(&etc_path, LINK_KEY) {
        Ok(Some(existing)) => {
            let existing_target = std::str::from_utf8(existing.as_slice())?;
            if existing_target == target.to_string_lossy() {
                // Link is unchanged.
                return Ok(());
            }
            fs::remove_file(&etc_path).map_err(|e| {
                eprintln!(
                    "Failed to remove existing file {}: {}",
                    etc_path.display(),
                    e
                );
                Box::new(e) as Box<dyn Error>
            })?;
        }
        Ok(None) => {
            if !migrate {
                eprintln!(
                    "File {} exists but has no symlink target!",
                    etc_path.display()
                );
                panic!(
                    "File {} exists but has no symlink target!",
                    etc_path.display()
                );
            } else {
                eprintln!("Migrating untracked symlink {}!", etc_path.display());

                if etc_path.exists() {
                    // Must be a user entity. Leave it untouched.
                    return Ok(());
                }
            }
        }
        Err(e) => {
            if e.raw_os_error() == Some(ENOENT) {
                // File doesn't exist, that's fine we'll create it.
            } else {
                eprintln!("Failed to get xattr for {}: {}", etc_path.display(), e);
                return Ok(()); // Whatever this path is, it's not for us!
            }
        }
    }

    etc_path
        .parent()
        .map(|p| fs::create_dir_all(p))
        .transpose()
        .map_err(|e| {
            eprintln!(
                "Failed to create parent directories for {}: {}",
                etc_path.display(),
                e
            );
            Box::new(e) as Box<dyn Error>
        })?;

    println!("Updating: {}", etc_path.display());

    symlink(&target, &etc_path)
        .or_else(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })
        .map_err(|e| {
            eprintln!(
                "Failed to create symlink {} -> {}: {}",
                etc_path.display(),
                target.display(),
                e
            );
            Box::new(e) as Box<dyn Error>
        })?;

    xattr::set(
        etc_path,
        LINK_KEY,
        target.to_str().unwrap_or_default().as_bytes(),
    )?;

    Ok(())
}

fn process(etc: &str, path: &Path, entry: DirEntry, migrate: &bool) -> Result<(), Box<dyn Error>> {
    let relative_path = path.strip_prefix(FACTORY).map_err(|e| {
        eprintln!(
            "Failed to strip prefix {} from {}: {}",
            FACTORY,
            path.display(),
            e
        );
        Box::new(e) as Box<dyn Error>
    })?;
    let etc_path = path::Path::new(etc).join(relative_path);

    if entry.metadata()?.is_symlink() {
        return process_symlink(path, etc_path, migrate);
    }

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let _ = io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();

    match xattr::get(&etc_path, HASH_KEY) {
        Ok(Some(existing)) => {
            let mut file = fs::File::open(&path)?;
            let mut hasher = Sha256::new();
            let _ = io::copy(&mut file, &mut hasher)?;
            let hash = hasher.finalize();

            if hash.as_slice() == existing {
                // Unchanged
                return Ok(());
            }
        }
        Ok(None) => {
            if !migrate {
                eprintln!("File {} exists but has no hash!", etc_path.display());
                panic!("File {} exists but has no hash!", etc_path.display());
            } else {
                eprintln!("Migrating untracked file {}!", etc_path.display());
                fs::remove_file(&etc_path).map_err(|e| {
                    eprintln!(
                        "Failed to remove existing file {}: {}",
                        etc_path.display(),
                        e
                    );
                    Box::new(e) as Box<dyn Error>
                })?;
            }
        }
        Err(e) => {
            if e.raw_os_error() == Some(ENOENT) {
                // File doesn't exist, that's fine we'll create it.
            } else {
                eprintln!("Failed to get xattr for {}: {}", etc_path.display(), e);
                return Ok(()); // Whatever this path is, it's not for us!
            }
        }
    }

    println!("Updating: {}", etc_path.display());

    etc_path
        .parent()
        .map(|p| fs::create_dir_all(p))
        .transpose()
        .map_err(|e| {
            eprintln!(
                "Failed to create parent directories for {}: {}",
                etc_path.display(),
                e
            );
            Box::new(e) as Box<dyn Error>
        })?;

    fs::copy(&path, &etc_path)?;

    xattr::set(etc_path, HASH_KEY, &hash)?;

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let migrate = &args.migrate;

    let etc = "/.etc.new/";
    create_subvolume(etc, migrate)?;

    let last_update = match xattr::get(etc, LAST_UPDATE_KEY) {
        Ok(Some(stamp)) => std::str::from_utf8(stamp.as_slice())?.parse::<u64>()?,
        Ok(None) => 0,
        Err(e) => {
            if e.raw_os_error() == Some(ENOENT) {
                0
            } else {
                return Err(Box::new(e));
            }
        }
    };

    let mut new_update: u64 = 0;
    for entry in WalkDir::new(FACTORY)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.metadata().unwrap().is_file() || e.metadata().unwrap().is_symlink())
    {
        let path = entry.path();

        if [".pwd.lock", "passwd", "shadow", "group", "localtime", "machine-id"].iter().any(|s| {
            path.file_name()
                .map(|f| f == *s)
                .unwrap_or(false)
        }) {
            // Never ever ever ever touch these files!
            // It's a bit silly to have them in the factory dir but whatever.
            println!("Skipping sensitive file: {}", path.display());
            continue;
        }

        let update = entry.metadata()?.modified()?.duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
        if last_update >= update {
            // File hasn't changed since last update, skip.
            continue;
        }
        new_update = max(new_update, update);

        process(etc, path, entry.clone(), migrate)?;
    }

    println!(
        "Last update was at {}, new update is at {}",
        last_update, new_update
    );

    if new_update == 0 || new_update == last_update {
        // Nothing was updated, nothing to do.
        return Ok(());
    }

    if args.replace {
        std::fs::rename("/etc", format!("/.etc.{}", SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs()))?;
        std::fs::rename(etc, "/etc")?;

        xattr::set("/etc", LAST_UPDATE_KEY, &new_update.to_string().as_bytes()).map_err(|e| {
            eprintln!("Failed to set last update xattr: {}", e);
            Box::new(e) as Box<dyn Error>
        })?;
    }

    Ok(())
}
