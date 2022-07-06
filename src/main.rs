use std::{collections::{HashMap, HashSet}, io::Error, hash::BuildHasher, process::{ExitStatus}};
use std::sync::{Arc, Mutex};

use warp::{http::Response, Filter};
use tokio::fs::{create_dir_all, metadata, remove_dir};
use tokio::process::{Command, Child};
use fnv::{FnvHashMap, FnvHashSet};
use futures::join;

const DEV_LOCATION: &str = if cfg!(feature = "docker") {
    "/mnt/docker/"
} else {
    "/dev/"
};

const UNIONFS_MOUNTPT: &str = "/var/www/localhost/htdocs";
const BASE_DIR: &str = "/root/base";

#[derive(PartialOrd, Ord, PartialEq, Eq)]
enum ProgressLevel {
    FoldersCreated,
    ZipMounted,
    FuzzyMounted,
    Mounted
}

struct HTTPResult {
    status: u16,
    message: String
}

struct MountStatus<T: BuildHasher> {
    mounted: HashSet<String, T>,
    changing: HashSet<String, T>,
    //mountString: String
}

struct LockedMountStatus<T: BuildHasher> {
    status: Mutex<MountStatus<T>>,
    union: Mutex<i32>
}

#[tokio::main]
async fn main() {
    let mut mount_status = LockedMountStatus {
        status: Mutex::new(MountStatus {
            mounted: FnvHashSet::default(),
            changing: FnvHashSet::default()
        }),
        union: Mutex::new(0)
    };
    //mount_status.mounted.insert("test".to_owned());
    let already_mounted = Arc::new(mount_status);
    //let mut already_mounted: Arc<Mutex<FnvHashSet<String>>> = Arc::new(Mutex::new(FnvHashSet::default()));
    // Match any request and return hello world!
    let routes = warp::path::end()
    .and(warp::query::<FnvHashMap<String, String>>())
    .map(|map: FnvHashMap<String, String>| {
        let mut response: Vec<String> = Vec::new();
        for (key, value) in map.into_iter() {
            response.push(format!("{}={}", key, value))
        }
        Response::builder().body(response.join(";"))
    });

    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
}


async fn mount_device<T: BuildHasher>(device_name: &str, shared_state: Arc<LockedMountStatus<T>>) -> HTTPResult {
    let devpath = DEV_LOCATION.to_owned() + device_name;
    let first_mp = "/tmp/".to_owned() + device_name;
    let second_mp = first_mp.clone() + ".fuzzy";
    let content = second_mp.clone() + "/content";
    match shared_state.status.lock() {
        Ok(mut mount_status) => {
            if mount_status.mounted.contains(&content) {
                return HTTPResult {
                    status: 304,
                    message: "Device is already mounted.".to_owned()
                }
            }
            if mount_status.changing.contains(&content) {
                return HTTPResult {
                    status: 409,
                    message: "Mount operation already in progress.".to_owned()
                }
            }
            mount_status.changing.insert(content.clone());
        }, Err(_) => {
            return HTTPResult {
                status: 500,
                message: "Failed to obtain state lock.".to_owned()
            };
        }
    }
    if metadata(&devpath).await.is_err() {
        if let Some(err) = remove_changing(&content, &shared_state) {
            return err;
        }
        return HTTPResult {
            status: 400,
            message: "Requested device doesn't exist: ".to_owned() + device_name
        };
    }
    // Use create_dir_all. Not because we expect /tmp to be missing, but because it won't throw an
    // error if the target path already exists.
    let dirs = join!(create_dir_all(&first_mp), create_dir_all(&second_mp));
    if dirs.0.is_err() || dirs.1.is_err() {
        if let Some(err) = remove_changing(&content, &shared_state) {
            return err;
        }
        return HTTPResult {
            status: 500,
            message: "Could not create mountpoints.".to_owned()
        };
    }

    // (sudo) fuse-archive /dev/sdb /tmp/sdb -o allow_other
    let zipmount = Command::new("fuse-archive")
    .arg(&devpath)
    .arg(&first_mp)
    .arg("-o")
    .arg("allow_other")
    .spawn();
    if let Some(err) = handle_subprocess(zipmount, &content, &shared_state).await {
        return err;
    }

    // (sudo) fuzzyfs /tmp/sdb /tmp/sdb.fuzzy -o allow_other
    let fuzzymount = Command::new("fuzzyfs")
    .arg(&first_mp)
    .arg(&second_mp)
    .arg("-o")
    .arg("allow_other")
    .spawn();
    if let Some(err) = handle_subprocess(fuzzymount, &content, &shared_state).await {
        return err;
    }
    
    // Check if the content folder exists.
    let meta_res = metadata(&content).await;
    match meta_res {
        Ok(meta) => {
            if !meta.is_dir() {
                unmount_device(shared_state, ProgressLevel::FuzzyMounted, first_mp, second_mp, content).await;
                return HTTPResult {
                    status: 500,
                    message: "No content folder.".to_owned()
                }
            }
        }, Err(_) => {
            unmount_device(shared_state, ProgressLevel::FuzzyMounted, first_mp, second_mp, content).await;
            return HTTPResult {
                status: 500,
                message: "No content folder.".to_owned()
            }
        }
    }

    // Yay, content folder exists!
    // The "union" is a mutex for controlling access to the unionfs mountpoint: /var/www/localhost/htdocs.
    // We wouldn't want multiple things to be mounting/unmounting unionfs at the same time - that could cause race conditions.
    // The lock also protects a number, because I couldn't figure out how to lock without data.
    match shared_state.union.lock() {
        Ok(mut count) => {
            // (sudo) umount -l /var/www/localhost/htdocs
            let umount = Command::new("umount")
            .arg("-l")
            .arg(UNIONFS_MOUNTPT)
            .spawn();
            if let Some(err) = handle_subprocess(umount, &content, &shared_state).await {
                return err;
            }

            // /root/base is always on top, and the current zip is directly after that.
            // Beyond that, we guarantee nothing about ordering.
            let mut mountlist: Vec<String> = vec![BASE_DIR.to_owned(), content.clone()];
            match shared_state.status.lock() {
                Ok(mount_status) => {
                    for key in &mount_status.mounted {
                        mountlist.push(key.clone());
                    }
                }, Err(_) => {
                    return HTTPResult {
                        status: 500,
                        message: "Failed to obtain state lock.".to_owned()
                    };
                }
            }

            // (sudo) unionfs /root/base:/mnt/sdb.fuzzy/content:/mnt/sda.fuzzy/content /var/www/localhost/htdocs -o allow_other
            let mount = Command::new("unionfs")
            .arg(mountlist.join(":"))
            .arg(UNIONFS_MOUNTPT)
            .arg("-o")
            .arg("allow_other")
            .spawn();
            if let Some(err) = handle_subprocess(mount, &content, &shared_state).await {
                return err;
            }

            // The zip is mounted! Move us from an inflight request to a completed one.
            match shared_state.status.lock() {
                Ok(mut mount_status) => {
                    mount_status.changing.remove(&content);
                    mount_status.mounted.insert(content);
                }, Err(_) => {
                    return HTTPResult {
                        status: 500,
                        message: "Failed to obtain state lock.".to_owned()
                    };
                }
            }
            // We have to use it so that it won't get dropped.
            *count += 1;
        }, Err(_) => {
            return HTTPResult {
                status: 500,
                message: "Failed to obtain union lock.".to_owned()
            };
        }
    }

    return HTTPResult {
        status: 200,
        message: "OK".to_owned()
    };
}

async fn unmount_device<T: BuildHasher>(shared_state: Arc<LockedMountStatus<T>>, progress: ProgressLevel, zip_mountpt: String, fuzzy_mountpt: String, union_mountpt: String) -> Option<HTTPResult> {
    if progress >= ProgressLevel::Mounted {
        // TODO remove from union and put inflight.
    }
    if progress >= ProgressLevel::FuzzyMounted {
        // (sudo) umount /tmp/sdb.fuzzy
        let fuzzy_unmount = Command::new("umount")
        .arg(&fuzzy_mountpt)
        .spawn();
        if let Some(err) = handle_subprocess(fuzzy_unmount, &union_mountpt, &shared_state).await {
            return Some(err);
        }
    }
    if progress >= ProgressLevel::ZipMounted {
        let zip_unmount = Command::new("umount")
        .arg(&zip_mountpt)
        .spawn();
        if let Some(err) = handle_subprocess(zip_unmount, &union_mountpt, &shared_state).await {
            return Some(err);
        }
    }
    if progress >= ProgressLevel::FoldersCreated {
        let dirs = join!(remove_dir(&fuzzy_mountpt), remove_dir(&zip_mountpt));
        if dirs.0.is_err() || dirs.1.is_err() {
            if let Some(err) = remove_changing(&union_mountpt, &shared_state) {
                return Some(err);
            }
            return Some(HTTPResult {
                status: 500,
                message: "Could not remove mountpoints.".to_owned()
            });
        }
    }
    return None;
}

fn remove_changing<T: BuildHasher>(key: &str, shared_state: &Arc<LockedMountStatus<T>>) -> Option<HTTPResult> {
    match shared_state.status.lock() {
        Ok(mut mount_status) => {
            mount_status.changing.remove(key);
            return None;
        }, Err(_) => {
            return Some(HTTPResult {
                status: 500,
                message: "Failed to obtain state lock.".to_owned()
            });
        }
    }
}

async fn handle_subprocess<T: BuildHasher>(spawnedproc: Result<Child, Error>, failure_key: &str, shared_state: &Arc<LockedMountStatus<T>>) -> Option<HTTPResult> {
    match spawnedproc {
        Ok(mut child) => {
            match child.wait().await {
                Ok(status_code) => {
                    if !status_code.success() {
                        if let Some(err) = remove_changing(failure_key, shared_state) {
                            return Some(err);
                        }
                        return Some(HTTPResult {
                            status: 500,
                            message: "Subprocess exited with an unsuccessful status.".to_owned()
                        });
                    }
                    return None;
                }, Err(_) => {
                    if let Some(err) = remove_changing(failure_key, shared_state) {
                        return Some(err);
                    }
                    return Some(HTTPResult {
                        status: 500,
                        message: "Could not read subprocess status.".to_owned()
                    });
                }
            }
        }, Err(_) => {
            if let Some(err) = remove_changing(failure_key, shared_state) {
                return Some(err);
            }
            return Some(HTTPResult {
                status: 500,
                message: "Could not spawn subprocess.".to_owned()
            });
        }
    }
}