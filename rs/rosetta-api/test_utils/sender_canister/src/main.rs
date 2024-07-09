use candid::{candid_method, CandidType, Principal};
use ic_cdk::{api::call::RejectionCode, update};
use serde::{Deserialize, Serialize};

#[derive(CandidType, Clone, Debug, Deserialize, Serialize)]
struct SendArg {
    to: Principal,
    method: String,
    arg: Vec<u8>,
    payment: u128,
}

#[update]
#[candid_method(update)]
async fn send(calls: Vec<SendArg>) -> Vec<Result<Vec<u8>, (RejectionCode, String)>> {
    let mut futures = vec![];
    for SendArg {
        to,
        method,
        arg,
        payment,
    } in calls
    {
        futures.push(ic_cdk::api::call::call_raw128(to, &method, arg, payment));
    }

    futures::future::join_all(futures).await
}

fn main() {}

candid::export_service!();

#[test]
fn check_candid_interface() {
    use candid_parser::utils::{service_equal, CandidSource};

    let new_interface = __export_service();
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let old_interface = manifest_dir.join("sender.did");
    service_equal(
        CandidSource::Text(&new_interface),
        CandidSource::File(old_interface.as_path()),
    )
    .unwrap_or_else(|e| {
        panic!(
            "the service interface is not compatible with {}: {:?}",
            old_interface.display(),
            e
        )
    });
}
