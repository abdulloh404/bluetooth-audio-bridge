use std::{env, path::PathBuf, process::Command};

fn run(command: &mut Command) {
    let status = command.status().expect("could not start native build tool");
    assert!(status.success(), "native build command failed: {command:?}");
}

fn main() {
    let source = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../../native/audio-engine");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("native");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=CXX");
    println!("cargo:rerun-if-env-changed=CMAKE_TOOLCHAIN_FILE");
    let mut configure = Command::new("cmake");
    configure.arg("-S").arg(&source).arg("-B").arg(&output)
        .arg("-DCMAKE_BUILD_TYPE=Release");
    if let Some(toolchain) = env::var_os("CMAKE_TOOLCHAIN_FILE") {
        configure.arg(format!("-DCMAKE_TOOLCHAIN_FILE={}", PathBuf::from(toolchain).display()));
    }
    run(&mut configure);
    run(Command::new("cmake").arg("--build").arg(&output));
    println!("cargo:rustc-link-search=native={}/lib", output.display());
    println!("cargo:rustc-link-lib=static=bt_audio_bridge_audio");
    let libraries = Command::new("pkg-config").args(["--libs", "libpipewire-0.3"])
        .output().expect("pkg-config is required for PipeWire");
    assert!(libraries.status.success(), "libpipewire-0.3 development files are required");
    for flag in String::from_utf8(libraries.stdout).unwrap().split_whitespace() {
        if let Some(name) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={name}");
        } else if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else {
            println!("cargo:rustc-link-arg={flag}");
        }
    }
    println!("cargo:rustc-link-lib=stdc++");
}
