use cargo_metadata::MetadataCommand;

fn main() {
    let metadata = MetadataCommand::new().exec().unwrap();

    // Find forward_system crate and expose its path to the directory containing `app*.bin` files.
    for package in &metadata.packages {
        if package.name.as_str() == "forward_system" {
            let forward_system_source = package.manifest_path.parent().unwrap();
            let zksync_os_source = forward_system_source.parent().unwrap().join("zksync_os");
            println!("cargo:rustc-env=ZKSYNC_OS_SOURCE_PATH={zksync_os_source}");
            break;
        }
    }
}
