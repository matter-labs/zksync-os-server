use std::fs::File;
use std::process::Command;
use std::str::from_utf8;

fn main() {
    // Rerun build script when test contracts
    println!("cargo::rerun-if-changed=test-contracts/src");
    println!("cargo::rerun-if-changed=test-contracts/foundry.toml");

    // Check that `forge` is installed and is executable
    let Ok(status) = Command::new("forge").arg("--version").status() else {
        println!("cargo::warning=`forge` not found, skipping build script");
        println!("cargo::warning=visit https://getfoundry.sh/ for installation instructions");
        return;
    };
    if !status.success() {
        println!("cargo::warning=could not run `forge --version`, skipping build script");
        println!("cargo::warning=make sure your foundry installation is working correctly");
        return;
    }

    match Command::new("forge")
        .arg("build")
        .arg("--root")
        .arg("test-contracts")
        .output()
    {
        Ok(output) if output.status.success() => {
            // Success, do nothing
        }
        Ok(output) => {
            println!("cargo::error=`forge build` failed, see stdout/stderr below");
            println!("cargo::error=stdout={}", from_utf8(&output.stdout).unwrap());
            println!("cargo::error=stderr={}", from_utf8(&output.stderr).unwrap());
        }
        Err(err) => {
            println!("cargo::error=could not run `forge build`: {err}");
        }
    }

    let dir = "prover-binaries";
    if !std::fs::exists(&dir).expect("failed to check dir existence") {
        std::fs::create_dir_all(dir).expect("failed to create dir");
    }
    let path = format!("{dir}/zksync_os_prover_service_v0_7_0");
    if !std::fs::exists(&path).expect("failed to check file existence") {
        let url = "https://github.com/matter-labs/zksync-airbender-prover/releases/download/v0.7.0/zksync_os_prover_service";
        let resp = reqwest::blocking::get(url).expect("failed to download");
        let body = resp.bytes().expect("failed to read response body").to_vec();
        std::fs::write(&path, body).expect("failed to write file");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let file = File::open(&path).expect("failed to open file");
            let mut perms = file
                .metadata()
                .expect("failed to load metadata")
                .permissions();
            perms.set_mode(0o755); // Sets rwxr-xr-x
            std::fs::set_permissions(&path, perms).expect("failed to set permissions");
        }
        #[cfg(not(unix))]
        {
            println!("cargo::error=unsupported platform (UNIX required)");
        }
    }
    println!("cargo:rustc-env=ZKSYNC_OS_PROVER_SERVICE_0_7_0_PATH={path}");
}
