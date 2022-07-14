use crate::HTTPResponse;
use core::future::Future;
use std::{collections::HashMap, hash::BuildHasher};
use urlencoding::decode;
use warp::{http::Response, reject::Rejection};

/// Handle a request to an endpoint that needs a devname param.
pub async fn handle_devname<
    U: BuildHasher,
    F: Fn(String) -> G,
    G: Future<Output = HTTPResponse>,
>(
    map: HashMap<String, String, U>,
    handle_param: F,
) -> Result<Response<String>, Rejection> {
    let builder = Response::builder();
    // Ensure that the "devname" param is set.
    if let Some(name) = map.get("devname") {
        if let Ok(decoded) = decode(name) {
            // If it is, mount the device.
            let mount_result = handle_param(decoded.into_owned()).await;
            //let mount_result = mount_device(&decoded, shared_state).await;
            // Return the resulting status and body.
            builder
                .status(mount_result.status)
                .body(mount_result.body)
                // Any parsing Errors (there will be none) get turned into Rejections.
                .map_err(|_| warp::reject())
        } else {
            builder
                .status(400)
                .body("Couldn't decode devname".to_owned())
                .map_err(|_| warp::reject())
        }
    } else {
        // Whoops, no "devname" param. Yell at the user.
        builder
            .status(400)
            .body("Required GET param absent: 'devname'".to_owned())
            .map_err(|_| warp::reject())
    }
}
