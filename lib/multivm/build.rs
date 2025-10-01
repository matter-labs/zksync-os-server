use cargo_metadata::MetadataCommand;

fn main() {
    let metadata = MetadataCommand::new().exec().unwrap();
    let versions = vec!["0.0.25"];

    // Find forward_system crate and expose its path to the directory containing `app*.bin` files.
    for package in &metadata.packages {
        for version in &versions {
            if package.name.as_str() == "forward_system"
            // && package.source.as_ref().is_some_and(|s| {
            //     s.to_string().contains(&format!(
            //         "https://github.com/matter-labs/zksync-os?tag=v{version}"
            //     ))
            // })
            {
                let forward_system_source = package.manifest_path.parent().unwrap();
                let zksync_os_source = forward_system_source.parent().unwrap().join("zksync_os");

                let snake_case_version = version.replace('.', "_");
                println!(
                    "cargo:rustc-env=ZKSYNC_OS_{snake_case_version}_SOURCE_PATH={zksync_os_source}"
                );
                break;
            }
        }
    }
}
