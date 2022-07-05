use std::{collections::{HashMap, HashSet}, io::Error, hash::BuildHasher, process::{ExitStatus}};
use std::sync::{Arc, Mutex};

use warp::{http::Response, Filter};
use tokio::fs::{create_dir, metadata};
use tokio::process::{Command, Child};
use fnv::{FnvHashMap, FnvHashSet};
use futures::join;

const DEV_LOCATION: &str = if cfg!(feature = "docker") {
    "/mnt/docker/"
} else {
    "/dev/"
};


struct HTTPResult {
    status: u16,
    message: String
}

struct MountStatus<T: BuildHasher> {
    mounted: HashSet<String, T>,
    changing: HashSet<String, T>
}

#[tokio::main]
async fn main() {
    let mut mount_status = MountStatus {
        mounted: FnvHashSet::default(),
        changing: FnvHashSet::default()
    };
    //mount_status.mounted.insert("test".to_owned());
    let already_mounted = Arc::new(Mutex::new(mount_status));
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


async fn mount_device<T: BuildHasher>(device_name: &str, shared_status: Arc<Mutex<MountStatus<T>>>) -> HTTPResult {
    let devpath = DEV_LOCATION.to_owned() + device_name;
    let first_mp = "/tmp/".to_owned() + device_name;
    let second_mp = first_mp.clone() + ".fuzzy";
    let content = second_mp.clone() + "/content";
    match shared_status.lock() {
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
        if let Some(err) = remove_changing(&content, &shared_status) {
            return err;
        }
        return HTTPResult {
            status: 400,
            message: "Requested device doesn't exist: ".to_owned() + device_name
        };
    }
    let dirs = join!(create_dir(&first_mp), create_dir(&second_mp));
    if dirs.0.is_err() || dirs.1.is_err() {
        if let Some(err) = remove_changing(&content, &shared_status) {
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
    if let Some(err) = handle_subprocess(zipmount, &content, &shared_status).await {
        return err;
    }

    // (sudo) fuzzyfs /tmp/sdb /tmp/sdb.fuzzy -o allow_other
    let fuzzymount = Command::new("fuzzyfs")
    .arg(&first_mp)
    .arg(&second_mp)
    .arg("-o")
    .arg("allow_other")
    .spawn();
    if let Some(err) = handle_subprocess(fuzzymount, &content, &shared_status).await {
        return err;
    }
    
    return HTTPResult {
        status: 200,
        message: "OK".to_owned()
    };
}

fn remove_changing<T: BuildHasher>(key: &str, shared_status: &Arc<Mutex<MountStatus<T>>>) -> Option<HTTPResult> {
    match shared_status.lock() {
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

async fn handle_subprocess<T: BuildHasher>(spawnedproc: Result<Child, Error>, failure_key: &str, shared_status: &Arc<Mutex<MountStatus<T>>>) -> Option<HTTPResult> {
    match spawnedproc {
        Ok(mut child) => {
            match child.wait().await {
                Ok(status_code) => {
                    if !status_code.success() {
                        if let Some(err) = remove_changing(failure_key, shared_status) {
                            return Some(err);
                        }
                        return Some(HTTPResult {
                            status: 500,
                            message: "Subprocess exited with an unsuccessful status.".to_owned()
                        });
                    }
                    return None;
                }, Err(_) => {
                    if let Some(err) = remove_changing(failure_key, shared_status) {
                        return Some(err);
                    }
                    return Some(HTTPResult {
                        status: 500,
                        message: "Could not read subprocess status.".to_owned()
                    });
                }
            }
        }, Err(_) => {
            if let Some(err) = remove_changing(failure_key, shared_status) {
                return Some(err);
            }
            return Some(HTTPResult {
                status: 500,
                message: "Could not spawn subprocess.".to_owned()
            });
        }
    }
}