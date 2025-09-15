// SPDX-License-Identifier: GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL
// SPDX-FileCopyrightText: 2025 Harald Sitter <sitter@kde.org>

use clap::Parser;
use core::panic;
use libbtrfsutil::{CreateSnapshotOptions, CreateSubvolumeOptions, DeleteSubvolumeOptions};
use libc::EINVAL;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::symlink;
use std::sync::{LazyLock, Mutex};
use std::{
    cmp::max,
    error::Error,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};
use walkdir::{DirEntry, WalkDir};

const CONFIG_FILE: &str = ".etc-factory.json";
const FACTORY_SYMLINK_COMPONENT: &str = "usr/share/factory/etc/"; // Trailing slash is important; we use this as input for strip_prefix

static SYSROOT: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(Args::parse().sysroot));

struct Paths {}

impl Paths {
    fn etc() -> String {
        SYSROOT.lock().expect("Failed to lock sysroot").to_owned() + ".etc.new/"
    }

    fn real_etc() -> String {
        SYSROOT.lock().expect("Failed to lock sysroot").to_owned() + "etc/"
    }

    fn factory() -> String {
        SYSROOT.lock().expect("Failed to lock sysroot").to_owned() + "usr/share/factory/etc/"
    }

    fn backup_etc() -> Result<String, Box<dyn Error>> {
        let time = format!(
            "/.etc.{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs()
        );
        Ok(SYSROOT.lock().expect("Failed to lock sysroot").to_owned() + time.as_str())
    }
}

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long)]
    migrate: bool,
    #[arg(long)]
    replace: bool,
    #[arg(long, default_value = "/")]
    sysroot: String,
}

#[derive(Serialize, Deserialize)]
struct FileEntry {
    hash: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct LinkEntry {
    target: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct EtcFactory {
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    seen: Vec<PathBuf>,

    version: u32,
    last_update: u64,
    files: std::collections::HashMap<PathBuf, FileEntry>,
    links: std::collections::HashMap<PathBuf, LinkEntry>,
}

impl EtcFactory {
    fn new(path: PathBuf) -> Result<EtcFactory, Box<dyn std::error::Error>> {
        let file = File::open(&path);
        let mut factory = match file {
            Ok(f) => serde_json::from_reader(f)?,
            Err(_) => EtcFactory {
                path: path.clone(),
                seen: Vec::new(),
                version: 1,
                last_update: 0,
                files: std::collections::HashMap::new(),
                links: std::collections::HashMap::new(),
            },
        };
        factory.path = path;
        Ok(factory)
    }

    fn write(&mut self, new_update: u64) -> Result<(), Box<dyn std::error::Error>> {
        self.last_update = new_update;
        let file = File::create(&self.path)?;
        serde_json::to_writer(file, &self)?;
        Ok(())
    }

    fn process_symlink(
        &mut self,
        relative_path: &Path,
        path: &Path,
        etc_path: PathBuf,
        migrate: &bool,
    ) -> Result<(), Box<dyn Error>> {
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
                self.links.insert(
                    relative_path.to_path_buf(),
                    LinkEntry {
                        target: target.clone(),
                    },
                );
            }
            return Ok(());
        }

        match self.links.get(&relative_path.to_path_buf()) {
            Some(existing) => {
                if existing.target == target {
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
            None => {
                if !migrate {
                    eprintln!(
                        "File {} exists but has no symlink target!",
                        etc_path.display()
                    );
                    panic!(
                        "File {} exists but has no symlink target!",
                        etc_path.display()
                    );
                } else if fs::read_link(&etc_path).is_ok() {
                    // This symlink exists in etc which means it is managed by the user already
                    // (if it wasn't it would have been removed earlier)
                    return Ok(());
                }
            }
        }

        match etc_path.parent().map(|p| fs::create_dir_all(p)).transpose() {
            Ok(_) => {}
            Err(e) => {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    return Ok(());
                }
                eprintln!(
                    "Failed to create parent directories for {}: {}",
                    etc_path.display(),
                    e
                );
                return Err(Box::new(e) as Box<dyn Error>);
            }
        }

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

        self.links.insert(
            relative_path.to_path_buf(),
            LinkEntry {
                target: target.clone(),
            },
        );

        Ok(())
    }

    fn process(
        &mut self,
        relative_path: &Path,
        etc: &Path,
        path: &Path,
        entry: DirEntry,
        migrate: &bool,
    ) -> Result<(), Box<dyn Error>> {
        let etc_path = etc.join(relative_path);

        if entry.metadata()?.is_symlink() {
            return self.process_symlink(relative_path, path, etc_path, migrate);
        }

        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let _ = io::copy(&mut file, &mut hasher)?;
        let hash = hasher.finalize();

        match self.files.get(&etc_path) {
            Some(existing) => {
                let mut file = fs::File::open(&path)?;
                let mut hasher = Sha256::new();
                let _ = io::copy(&mut file, &mut hasher)?;
                let hash = hasher.finalize();

                if hash.as_slice() == existing.hash.as_slice() {
                    // Unchanged
                    return Ok(());
                }
            }
            None => {
                if !migrate {
                    eprintln!("File {} exists but has no hash!", etc_path.display());
                    panic!("File {} exists but has no hash!", etc_path.display());
                } else if etc_path.exists() {
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
        }

        println!("Updating: {}", etc_path.display());

        match etc_path.parent().map(|p| fs::create_dir_all(p)).transpose() {
            Ok(_) => {}
            Err(e) => {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    return Ok(());
                }

                eprintln!(
                    "Failed to create parent directories for {}: {}",
                    etc_path.display(),
                    e
                );
                return Err(Box::new(e) as Box<dyn Error>);
            }
        }

        fs::copy(&path, &etc_path)?;

        self.files.insert(
            relative_path.to_path_buf(),
            FileEntry {
                hash: hash.to_vec(),
            },
        );

        Ok(())
    }

    fn force_update(&mut self, new_update: u64) -> Result<u64, Box<dyn Error>> {
        let update = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        return Ok(max(new_update, update));
    }

    fn process_deleted_files(
        &mut self,
        etc: &Path,
        new_update: u64,
    ) -> Result<u64, Box<dyn Error>> {
        let mut to_drop = Vec::new();
        for (relative_path, info) in &self.files {
            if self.seen.contains(relative_path) {
                // still around
                continue;
            }
            to_drop.push(relative_path.clone());

            let path = etc.join(relative_path);

            let mut file = File::open(&path)?;
            let mut hasher = Sha256::new();
            let _ = io::copy(&mut file, &mut hasher)?;
            let hash = hasher.finalize();

            if hash.as_slice() != info.hash.as_slice() {
                // File was changed by user. Don't remove it.
                continue;
            }

            println!("Removing dangling file: {}", path.display());
            fs::remove_file(path)?;
        }

        // Mind that dropping means this file is no longer present in factory.
        // Whether or not it was removed from etc is a different question.
        for path in &to_drop {
            self.files.remove(path);
        }

        Ok(to_drop
            .is_empty()
            .then(|| new_update)
            .unwrap_or(self.force_update(new_update)?))
    }

    fn process_deleted_symlinks(
        &mut self,
        etc: &Path,
        new_update: u64,
    ) -> Result<u64, Box<dyn Error>> {
        let mut to_drop = Vec::new();
        for (relative_path, info) in &self.links {
            if self.seen.contains(relative_path) {
                // still around
                continue;
            }
            to_drop.push(relative_path.clone());

            let path = etc.join(relative_path);

            if fs::read_link(&path).unwrap_or_default() != info.target {
                // Link was changed by user. Don't remove it.
                continue;
            }

            println!("Removing dangling link: {}", path.display());
            fs::remove_file(path)?;
        }

        // Mind that dropping means this file is no longer present in factory.
        // Whether or not it was removed from etc is a different question.
        for path in &to_drop {
            self.links.remove(path);
        }

        Ok(to_drop
            .is_empty()
            .then(|| new_update)
            .unwrap_or(self.force_update(new_update)?))
    }

    fn process_all(&mut self, etc: &Path, migrate: &bool) -> Result<u64, Box<dyn Error>> {
        let mut new_update: u64 = 0;
        for entry in WalkDir::new(Paths::factory())
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.metadata().unwrap().is_file() || e.metadata().unwrap().is_symlink())
        {
            let path = entry.path();
            // TODO pass into process function
            let relative_path = path.strip_prefix(Paths::factory()).map_err(|e| {
                eprintln!(
                    "Failed to strip prefix {} from {}: {}",
                    Paths::factory(),
                    path.display(),
                    e
                );
                Box::new(e) as Box<dyn Error>
            })?;
            self.seen.push(relative_path.to_path_buf());

            if [
                ".pwd.lock",
                "passwd",
                "passwd-",
                "shadow",
                "shadow-",
                "gshadow",
                "gshadow-",
                "group",
                "group-",
                "localtime",
                "machine-id",
                "fstab",
                "subuid",
                "subgid-",
                "subgid",
                "subgid-",
                "crypttab",
                "vconsole.conf",
                "hostname",
                "hosts",
                "locale.conf",
                "locale.gen",
            ]
            .iter()
            .any(|s| path.file_name().map(|f| f == *s).unwrap_or(false))
            {
                // Never ever ever ever touch these files!
                // It's a bit silly to have them in the factory dir but whatever.
                println!("Skipping sensitive file: {}", path.display());
                continue;
            }

            let update = entry
                .metadata()?
                .modified()?
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs();
            if self.last_update >= update {
                // File hasn't changed since last update, skip.
                continue;
            }
            new_update = max(new_update, update);

            self.process(relative_path, etc, path, entry.clone(), migrate)?;
        }

        new_update = max(new_update, self.process_deleted_files(etc, new_update)?);
        new_update = max(new_update, self.process_deleted_symlinks(etc, new_update)?);

        Ok(new_update)
    }
}

fn seed_subvolume(etc: &str) -> Result<(), Box<dyn Error>> {
    CreateSubvolumeOptions::new().create(etc).map_err(|e| {
        eprintln!("Failed to create subvolume: {}", e);
        Box::new(e) as Box<dyn Error>
    })?;

    Command::new("cp")
        .arg("--archive")
        .arg("--recursive")
        .arg(Paths::real_etc().to_owned() + "/.")
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
        println!("Symlink target: {}", target.display());
        if target.to_string_lossy().contains(FACTORY_SYMLINK_COMPONENT) {
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
    println!("Creating snapshot of {} at {}", Paths::real_etc(), etc);
    CreateSnapshotOptions::new()
        .readonly(false)
        .recursive(true)
        .create(Paths::real_etc(), etc)
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

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let migrate = &args.migrate;

    let etc_str = &Paths::etc();
    let etc = Path::new(etc_str);
    create_subvolume(&etc_str, migrate)?;

    let mut factory = EtcFactory::new(etc.join(CONFIG_FILE))?;
    let new_update = factory.process_all(etc, migrate)?;

    println!(
        "Last update was at {}, new update is at {}",
        factory.last_update, new_update
    );

    if new_update == 0 || new_update == factory.last_update {
        // Nothing was updated, nothing to do.
        return Ok(());
    }

    let _ = factory.write(new_update);

    if args.replace {
        std::fs::rename(Paths::real_etc(), Paths::backup_etc()?)?;
        std::fs::rename(etc, Paths::real_etc())?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::{env, error::Error, fs::File, thread::sleep};

    #[test]
    fn test_0_processor() -> Result<(), Box<dyn Error>> {
        *SYSROOT.lock()? = env::current_dir()?.to_string_lossy().to_string() + "/mnt/";

        create_subvolume(&Paths::etc(), &true)?;

        assert!(Path::new(&Paths::etc()).exists());
        assert!(Path::new(&Paths::real_etc()).exists());
        assert!(!Path::new(&Paths::etc()).join("asymlinktofactory").exists());

        let etc_str = &Paths::etc();
        let etc = Path::new(etc_str);
        let migrate = &true;

        let mut factory = EtcFactory::new(etc.join(CONFIG_FILE))?;
        let new_update = factory.process_all(etc, migrate)?;
        factory.write(new_update)?;

        assert!(Path::new(&Paths::etc()).exists());
        assert!(Path::new(&Paths::real_etc()).exists());
        // Should get cleaned up
        assert!(!Path::new(&Paths::etc()).join("asymlinktofactory").exists());

        assert!(new_update > 0);

        let file = File::open(Path::new(&Paths::etc()).join(CONFIG_FILE))?;
        let factory: EtcFactory = serde_json::from_reader(file)?;
        assert_eq!(factory.last_update, new_update);

        factory.files.iter().for_each(|(k, v)| {
            println!("File: {} Hash: {:x?}", k.display(), v.hash);
        });

        factory.links.iter().for_each(|(k, v)| {
            println!("Link: {} Target: {}", k.display(), v.target.display());
        });

        // adir/morefile1 (was already there)
        // adir/morefile2 (was already there)
        // afile (was already there)
        // anotherfile (new from factory)
        assert_eq!(factory.files.keys().len(), 4);
        // arelativelink (was already there)
        assert_eq!(factory.links.keys().len(), 1);

        // Remove dangling file ------------------------------------------------

        fs::remove_file(Path::new(&Paths::factory()).join("arelativelink")).ok();

        sleep(std::time::Duration::from_secs(1)); // Ensure time changes
        let mut factory = EtcFactory::new(etc.join(CONFIG_FILE))?;
        let new_new_update = factory.process_all(etc, migrate)?;

        assert_ne!(new_update, new_new_update);
        assert_eq!(factory.files.keys().len(), 4);
        // Link has been removed as dangling
        assert_eq!(factory.links.keys().len(), 0);

        Ok(())
    }
}
