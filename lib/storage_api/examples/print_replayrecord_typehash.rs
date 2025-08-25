use type_hash::TypeHash;
use zksync_os_storage_api::ReplayRecord;

fn main() {
    println!("{}", ReplayRecord::type_hash());
}
