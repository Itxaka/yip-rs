use std::process::Command;

fn main() {
    let version = std::env::var("YIP_VERSION")
        .or_else(|_| {
            Command::new("git")
                .args(["describe", "--tags", "--always", "--dirty"])
                .output()
                .map_err(|e| e.to_string())
                .and_then(|o| String::from_utf8(o.stdout).map_err(|e| e.to_string()))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|_| "unknown".to_string());

    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=YIP_VERSION={version}");
    println!("cargo:rustc-env=YIP_COMMIT={commit}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-env-changed=YIP_VERSION");
}
