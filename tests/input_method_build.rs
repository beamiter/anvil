//! This test can also run without rebuilding GTK dependencies:
//! rustc --edition 2021 --test tests/input_method_build.rs -o /tmp/ime-build-test
//! /tmp/ime-build-test
#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_build(binary: &Path, root: &Path, extra: &[(&str, &std::ffi::OsStr)]) -> String {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("HOME", root)
        .env("CARGO_CFG_TARGET_OS", "linux")
        .env("HOST", "x86_64-unknown-linux-gnu")
        .env("TARGET", "x86_64-unknown-linux-gnu")
        .env("PKG_CONFIG", root.join("pkg-config"));
    for (name, value) in extra {
        command.env(name, value);
    }
    let output = command.output().expect("run input-method build script");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn cargo_build_retains_input_method_discovery_without_shell_hook() {
    let root = Fixture(env::temp_dir().join(format!(
            "ime-build-integration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    fs::create_dir_all(&root.0).unwrap();
    let source = Path::new(file!()).parent().unwrap().join("../build.rs");
    let binary = root.0.join("build-script");
    let compiled = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .args(["--edition", "2021"])
        .arg(source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    // Keep this independent of the developer's pkg-config and IM environment.
    let pkg_config = root.0.join("pkg-config");
    fs::write(
        &pkg_config,
        "#!/bin/sh\nprintf '%s\\n' /nix/store/fixture-gtk4/lib\n",
    )
    .unwrap();
    fs::set_permissions(&pkg_config, fs::Permissions::from_mode(0o700)).unwrap();
    let module = root.0.join("valid-module");
    fs::create_dir_all(module.join("4.0.0/immodules")).unwrap();
    fs::write(module.join("4.0.0/immodules/libim-fcitx5.so"), []).unwrap();
    let missing = root.0.join("missing-module");
    let paths = env::join_paths([&missing, &module]).unwrap();
    let found = run_build(&binary, &root.0, &[("GTK_PATH", &paths)]);
    assert!(found.contains(&format!(
        "cargo:rustc-env=FCITX5_GTK_PATH={}\n",
        module.display()
    )));
    assert!(!found.contains(&format!(
        "cargo:rustc-env=FCITX5_GTK_PATH={}\n",
        missing.display()
    )));

    // Explicit configuration is inherited by rustc rather than replaced with
    // discovery. Even a path absent on the build host belongs to the caller.
    let explicit = root.0.join("explicit-target-module");
    let preserved = run_build(
        &binary,
        &root.0,
        &[
            ("GTK_PATH", &paths),
            ("FCITX5_GTK_PATH", explicit.as_os_str()),
        ],
    );
    assert!(!preserved.contains("cargo:rustc-env=FCITX5_GTK_PATH="));

    let cross = run_build(
        &binary,
        &root.0,
        &[
            ("GTK_PATH", &paths),
            ("TARGET", std::ffi::OsStr::new("aarch64-unknown-linux-gnu")),
        ],
    );
    assert!(
        !cross.contains("cargo:rustc-env=FCITX5_GTK_PATH="),
        "cross builds must not embed a host module"
    );

    use std::os::unix::ffi::OsStringExt;
    for name in [
        std::ffi::OsString::from("module\nnewline"),
        std::ffi::OsString::from_vec(b"module-\xff".to_vec()),
    ] {
        let unsafe_path = root.0.join(name);
        fs::create_dir_all(unsafe_path.join("4.0.0/immodules")).unwrap();
        fs::write(unsafe_path.join("4.0.0/immodules/libim-fcitx5.so"), []).unwrap();
        let rejected = run_build(&binary, &root.0, &[("GTK_PATH", unsafe_path.as_os_str())]);
        assert!(!rejected.contains("cargo:rustc-env=FCITX5_GTK_PATH="));
        assert!(rejected.contains("cargo:warning=The discovered fcitx GTK4 module path"));
    }

    // This is the original regression: both module-path variables are absent,
    // but pkg-config selects Nix GTK and its fcitx module is already installed.
    let discovered = run_build(&binary, &root.0, &[]);
    let installed = fs::read_dir("/nix/store")
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.file_name().to_string_lossy().contains("-fcitx5-gtk-")
                && entry
                    .path()
                    .join("lib/gtk-4.0/4.0.0/immodules/libim-fcitx5.so")
                    .is_file()
        });
    if installed {
        let selected = discovered
            .lines()
            .find_map(|line| line.strip_prefix("cargo:rustc-env=FCITX5_GTK_PATH="))
            .expect("Nix GTK Cargo builds must retain the installed IM module path");
        assert!(Path::new(selected)
            .join("4.0.0/immodules/libim-fcitx5.so")
            .is_file());
        assert!(
            Path::new(selected).starts_with("/nix/store"),
            "Nix GTK discovery must prefer its own module over implicit system modules"
        );
    } else {
        eprintln!("Nix-store fallback needs an installed fcitx5-gtk; override and invalid-path cases were verified");
    }
}
