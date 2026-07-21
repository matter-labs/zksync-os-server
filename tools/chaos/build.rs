use std::process::Command;
use std::str::from_utf8;

/// Compile the workload contracts (`contracts/`) with forge so `chaos load` can
/// read their deployment bytecode at runtime. Missing forge downgrades to a
/// warning — the crate itself compiles fine without artifacts, and the load
/// command explains what to install if a contract workload is then requested.
fn main() {
    println!("cargo::rerun-if-changed=contracts/src");
    println!("cargo::rerun-if-changed=contracts/foundry.toml");

    let Ok(status) = Command::new("forge").arg("--version").status() else {
        println!(
            "cargo::warning=`forge` not found; contract workloads need it (https://getfoundry.sh/)"
        );
        return;
    };
    if !status.success() {
        println!("cargo::warning=could not run `forge --version`; skipping contract build");
        return;
    }

    match Command::new("forge")
        .arg("build")
        .arg("--root")
        .arg("contracts")
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            println!("cargo::error=`forge build` of the chaos contracts failed");
            println!(
                "cargo::error=stdout={}",
                from_utf8(&output.stdout).unwrap_or("")
            );
            println!(
                "cargo::error=stderr={}",
                from_utf8(&output.stderr).unwrap_or("")
            );
        }
        Err(err) => {
            println!("cargo::error=could not run `forge build`: {err}");
        }
    }
}
