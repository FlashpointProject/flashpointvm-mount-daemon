use std::{collections::HashSet, hash::BuildHasher};

use fnv::{FnvBuildHasher, FnvHashMap, FnvHashSet};
use lazy_static::lazy_static;
use parking_lot::Mutex;
use tokio::fs::{create_dir_all, metadata, remove_dir};
use tokio::join;
use tokio::process::{Child, Command};
use warp::Filter;

mod util;
use util::handle_devname;

const DEV_LOCATION: &str = if cfg!(feature = "docker") {
    "/mnt/docker/"
} else {
    "/dev/"
};

const UNIONFS_MOUNTPT: &str = "/var/www/localhost/htdocs";
const BASE_DIR: &str = "/root/base";

// Binary paths, hard-coded for alpine. Modify to taste.
const FUSE_ARCHIVE: &str = "/usr/local/bin/fuse-archive";
const FUZZYFS: &str = "/usr/local/bin/fuzzyfs";
const UMOUNT: &str = "/bin/umount";
const UNIONFS: &str = "/usr/bin/unionfs";

pub struct HTTPResponse {
    status: u16,
    body: String,
}

struct MountStatus<T: BuildHasher> {
    mounted: HashSet<String, T>,
    changing: HashSet<String, T>,
}

lazy_static! {
    static ref MOUNT_STATUS: Mutex<MountStatus<FnvBuildHasher>> = Mutex::new(MountStatus {
        mounted: FnvHashSet::default(),
        changing: FnvHashSet::default(),
    });
}

lazy_static! {
    static ref UNION_MUTEX: tokio::sync::Mutex<i32> = tokio::sync::Mutex::new(0);
}

#[tokio::main]
async fn main() {
    // Create the "/mount" route.
    let mount = warp::path("mount")
        // It ends at /mount, no further path params.
        .and(warp::path::end())
        // It takes a GET param.
        .and(warp::query::<FnvHashMap<String, String>>())
        // We use and_then instead of map, because this needs async capabilities.
        .and_then(move |map: FnvHashMap<String, String>| {
            // Increase the refcount for the global state.
            async move {
                return handle_devname(map, mount_device).await;
            }
        });
    // Pretty much the same as the previous one, not going to repeat all the comments.
    let umount = warp::path("umount")
        .and(warp::path::end())
        .and(warp::query::<FnvHashMap<String, String>>())
        .and_then(move |map: FnvHashMap<String, String>| async move {
            return handle_devname(map, umount_device).await;
        });

    // Merge the routes into a single thing.
    let routes = warp::get().and(mount).or(umount);

    // Serve on port 3030. Let's hope this works.
    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
}

/// Mounts a device, specified by the device's filename in `DEV_LOCATION`.
async fn mount_device(device_name: String) -> HTTPResponse {
    // Construct some useful strings.
    // The path to the device.
    let devpath = DEV_LOCATION.to_owned() + &device_name;
    // The fuse-archive mountpoint.
    let zip_mountpt = "/tmp/".to_owned() + &device_name;
    // The fuzzyfs mountpoint.
    let fuzzy_mountpt = zip_mountpt.clone() + ".fuzzy";
    // The location of the content folder inside the fuzzyfs mount.
    // This will be used to construct the union mount. It's also used as a unique ID for this device.
    let content = fuzzy_mountpt.clone() + "/content";

    // Check: does the device exist?
    match metadata(&devpath).await {
        Ok(meta) => {
            // Path exists, check that it's not a directory. Other than that, we're good to go.
            if meta.is_dir() {
                return HTTPResponse {
                    status: 400,
                    body: "Requested device is a directory : ".to_owned() + &device_name,
                };
            }
        }
        // Device doesn't exist.
        Err(_) => {
            return HTTPResponse {
                status: 400,
                body: "Requested device doesn't exist: ".to_owned() + &device_name,
            };
        }
    }

    // Verify that it's safe to proceed with mounting this device.
    // We wouldn't want to attempt a mount if:
    //  - The device is already mounted.
    //  - The device is being mounted/unmounted by another request.
    // So, we synchronize with some shared state.
    {
        let mut mount_status = MOUNT_STATUS.lock();
        // Is it already mounted?
        if mount_status.mounted.contains(&content) {
            return HTTPResponse {
                status: 200,
                body: "Device is already mounted.".to_owned(),
            };
        }
        // Is a mount operation currently in progess?
        if mount_status.changing.contains(&content) {
            return HTTPResponse {
                status: 409,
                body: "Mount operation already in progress.".to_owned(),
            };
        }
        // Checks passed, it's safe to proceed. Mark this device as in-progress.
        mount_status.changing.insert(content.clone());
    }

    // Create the mountmounts in /tmp. For creating folders, we use create_dir_all.
    // This is not because we expect /tmp to be missing, but because it won't throw an
    // error if the target path already exists.
    let dirs = join!(create_dir_all(&zip_mountpt), create_dir_all(&fuzzy_mountpt));
    if dirs.0.is_err() || dirs.1.is_err() {
        remove_changing(&content);
        return HTTPResponse {
            status: 500,
            body: "Could not create mountpoints.".to_owned(),
        };
    }

    // Perform the fuse-archive mount.
    // (sudo) fuse-archive /dev/sdb /tmp/sdb -o allow_other
    let zipmount = Command::new(FUSE_ARCHIVE)
        .arg(&devpath)
        .arg(&zip_mountpt)
        .arg("-o")
        .arg("allow_other")
        .spawn();
    if let Some(err) = handle_subprocess(zipmount, &content).await {
        return err;
    }

    // Perform the fuzzyfs mount.
    // (sudo) fuzzyfs /tmp/sdb /tmp/sdb.fuzzy -o allow_other
    let fuzzymount = Command::new(FUZZYFS)
        .arg(&zip_mountpt)
        .arg(&fuzzy_mountpt)
        .arg("-o")
        .arg("allow_other")
        .spawn();
    if let Some(err) = handle_subprocess(fuzzymount, &content).await {
        // If we can't reliably spawn subprocesses, no point in trying to unmount the zip mount.
        // This will be a code 500 anyway, that should be enough for people to get the idea that
        // something went wrong.
        return err;
    }

    // Check if the content folder exists.
    let meta_res = metadata(&content).await;
    let mut content_exists = true;
    match meta_res {
        Ok(meta) => {
            if !meta.is_dir() {
                // Something called "content" exists, but it's not what we're looking for.
                content_exists = false;
            }
        }
        Err(_) => {
            // The path doesn't exist.
            content_exists = false;
        }
    }
    // It doesn't exist. As part of clean-up, we unmount the things we mounted a moment ago.
    if !content_exists {
        if let Some(err) = cleanup_mount(&zip_mountpt, &fuzzy_mountpt, &content).await {
            return err;
        }
        return HTTPResponse {
            status: 500,
            body: "No content folder.".to_owned(),
        };
    }

    // The content folder exists! Now we mount it to the unionfs mount.
    // UNION_MUTEX is a mutex for controlling access to the unionfs mountpoint: /var/www/localhost/htdocs.
    // We wouldn't want multiple things to be mounting/unmounting unionfs at the same time - that could cause race conditions.
    // The lock also protects a number, because I couldn't figure out how to lock without data.
    {
        let mut count = UNION_MUTEX.lock().await;
        // Unmount the current unionfs.
        // (sudo) umount -l /var/www/localhost/htdocs
        let umount = Command::new(UMOUNT).arg("-l").arg(UNIONFS_MOUNTPT).spawn();
        if let Some(err) = handle_subprocess(umount, &content).await {
            return err;
        }

        // Grab the currently-mounted objects. Note that this is safe to unlock, because
        // any modifiers of mount_status.mounted will also be holding the union lock.
        // /root/base is always on top, and the current zip is directly after that.
        // Beyond that, we guarantee nothing about ordering. Honestly, people should be
        // using the umount api after a game closes anyway.
        let mut mountlist: Vec<String> = vec![BASE_DIR.to_owned(), content.clone()];
        {
            let mount_status = MOUNT_STATUS.lock();
            for key in &mount_status.mounted {
                // PERF: zero-copy?
                mountlist.push(key.clone());
            }
        }

        // Remount the unionfs mount.
        // (sudo) unionfs /root/base:/tmp/sdb.fuzzy/content:/tmp/sda.fuzzy/content /var/www/localhost/htdocs -o allow_other
        let mount = Command::new(UNIONFS)
            .arg(mountlist.join(":"))
            .arg(UNIONFS_MOUNTPT)
            .arg("-o")
            .arg("allow_other")
            .spawn();
        if let Some(err) = handle_subprocess(mount, &content).await {
            return err;
        }

        // The zip is mounted! Move this device's status from changing (inflight) to mounted.
        {
            let mut mount_status = MOUNT_STATUS.lock();
            mount_status.changing.remove(&content);
            mount_status.mounted.insert(content);
        }
        // We have to use it so that it won't get dropped - the mutex unlocks on-drop.
        *count += 1;
    }

    // Yay, we made it!
    HTTPResponse {
        status: 201,
        body: "OK".to_owned(),
    }
}

/// Unmounts a device, specified by the device's filename in `DEV_LOCATION`.
async fn umount_device(device_name: String) -> HTTPResponse {
    // Construct some useful strings.
    // The fuse-archive mountpoint.
    let zip_mountpt = "/tmp/".to_owned() + &device_name;
    // The fuzzyfs mountpoint.
    let fuzzy_mountpt = zip_mountpt.clone() + ".fuzzy";
    // The location of the content folder inside the fuzzyfs mount.
    // This will be used to construct the union mount. It's also used as a unique ID for this device.
    let content = fuzzy_mountpt.clone() + "/content";

    // Verify that this device is indeed mounted. We wouldn't want to try unmounting a device that isn't mounted.
    {
        let mut mount_status = MOUNT_STATUS.lock();
        if mount_status.changing.contains(&content) {
            return HTTPResponse {
                status: 409,
                body: "Mount operation already in progress.".to_owned(),
            };
        }
        if !mount_status.mounted.contains(&content) {
            return HTTPResponse {
                status: 200,
                body: "Device is not mounted.".to_owned(),
            };
        }
        // Set the status to changing *before* we do anything to avoid race conditions.
        mount_status.mounted.remove(&content);
        mount_status.changing.insert(content.clone());
    }

    // Okay, it's mounted. Time to unmount it.
    {
        // Acquire the async union lock.
        let mut count = UNION_MUTEX.lock().await;

        // Unmount the current unionfs.
        // (sudo) umount -l /var/www/localhost/htdocs
        let umount = Command::new(UMOUNT).arg("-l").arg(UNIONFS_MOUNTPT).spawn();
        if let Some(err) = handle_subprocess(umount, &content).await {
            return err;
        }

        // Change the status from mounted to changing, and pick up a list of mounted zips at the same time.
        let mut mountlist: Vec<String> = vec![BASE_DIR.to_owned()];
        {
            let mount_status = MOUNT_STATUS.lock();
            for key in &mount_status.mounted {
                mountlist.push(key.clone());
            }
        }

        // Remount the unionfs mount.
        // (sudo) unionfs /root/base:/tmp/sda.fuzzy/content /var/www/localhost/htdocs -o allow_other
        let mount = Command::new(UNIONFS)
            .arg(mountlist.join(":"))
            .arg(UNIONFS_MOUNTPT)
            .arg("-o")
            .arg("allow_other")
            .spawn();
        if let Some(err) = handle_subprocess(mount, &content).await {
            return err;
        }

        // Modify it at the end so that the lock won't get dropped.
        *count -= 1;
    }
    // We've successfully removed it from the union mount, continue to the other
    // unmounting steps.
    if let Some(err) = cleanup_mount(&zip_mountpt, &fuzzy_mountpt, &content).await {
        return err;
    }

    // Yay, we did it!
    HTTPResponse {
        status: 201,
        body: "OK".to_owned(),
    }
}

/// Cleans up a non-unioned device mount. Always removes the `union_mountpt` from `MOUNT_STATUS`.
async fn cleanup_mount(
    zip_mountpt: &str,
    fuzzy_mountpt: &str,
    union_mountpt: &str,
) -> Option<HTTPResponse> {
    // Unmount the fuzzyfs mount.
    // (sudo) umount /tmp/sdb.fuzzy
    let fuzzy_unmount = Command::new(UMOUNT).arg(fuzzy_mountpt).spawn();
    if let Some(err) = handle_subprocess(fuzzy_unmount, union_mountpt).await {
        return Some(err);
    }

    // Unmount the fuse-archive mount.
    let zip_unmount = Command::new(UMOUNT).arg(zip_mountpt).spawn();
    if let Some(err) = handle_subprocess(zip_unmount, union_mountpt).await {
        return Some(err);
    }

    // Delete the mount points.
    let dirs = join!(remove_dir(fuzzy_mountpt), remove_dir(zip_mountpt));
    if dirs.0.is_err() || dirs.1.is_err() {
        remove_changing(union_mountpt);
        return Some(HTTPResponse {
            status: 500,
            body: "Could not remove mountpoints.".to_owned(),
        });
    }
    // Remove the inflight marker for this device.
    remove_changing(union_mountpt);
    None
}

/// Removes a key from the shared state's `changing` hashset.
fn remove_changing(key: &str) {
    let mut mount_status = MOUNT_STATUS.lock();
    mount_status.changing.remove(key);
}

/// Wait for a process to spawn and exit, and handle any errors that result.
async fn handle_subprocess(
    spawnedproc: std::io::Result<Child>,
    failure_key: &str,
) -> Option<HTTPResponse> {
    match spawnedproc {
        // Did it spawn successfully?
        Ok(mut child) => {
            // Yup, wait for it to complete.
            match child.wait().await {
                Ok(status_code) => {
                    // Check that it was successful.
                    if !status_code.success() {
                        remove_changing(failure_key);
                        return Some(HTTPResponse {
                            status: 500,
                            body: "Subprocess exited with an unsuccessful status.".to_owned(),
                        });
                    }
                    None
                }
                Err(_) => {
                    remove_changing(failure_key);
                    Some(HTTPResponse {
                        status: 500,
                        body: "Could not read subprocess status.".to_owned(),
                    })
                }
            }
        }
        Err(_) => {
            remove_changing(failure_key);
            Some(HTTPResponse {
                status: 500,
                body: "Could not spawn subprocess.".to_owned(),
            })
        }
    }
}
