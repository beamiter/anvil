use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const MODULES: [&str; 4] = [
    "libim-fcitx5.so",
    "im-fcitx5.so",
    "libim-fcitx.so",
    "im-fcitx.so",
];

fn has_module(path: &Path) -> bool {
    MODULES
        .iter()
        .any(|name| path.join("4.0.0/immodules").join(name).is_file())
}

fn first_module(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    paths.into_iter().find(|path| has_module(path))
}

fn nix_module_paths(store: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(store)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().contains("-fcitx5-gtk-"))
        .map(|entry| entry.path().join("lib/gtk-4.0"))
        .filter(|path| has_module(path))
        .collect();
    paths.sort();
    paths
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for name in [
        "FCITX5_GTK_PATH",
        "GTK_PATH",
        "PKG_CONFIG",
        "PKG_CONFIG_PATH",
        "PKG_CONFIG_LIBDIR",
        "HOME",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    // Nix package builds and explicit caller configuration remain authoritative.
    if env::var_os("FCITX5_GTK_PATH").is_some_and(|value| !value.is_empty()) {
        return;
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    // Automatic discovery must never capture build-host modules in a target
    // binary. Cross builds can provide an explicit target module path above.
    if env::var_os("HOST") != env::var_os("TARGET") {
        return;
    }
    let mut candidates: Vec<_> = env::var_os("GTK_PATH")
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default();
    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".nix-profile/lib/gtk-4.0"));
    }
    let path = first_module(candidates).or_else(|| {
        let nix_gtk = Command::new(env::var_os("PKG_CONFIG").unwrap_or_else(|| "pkg-config".into()))
            .args(["--variable=libdir", "gtk4"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .is_some_and(|libdir| Path::new(libdir.trim()).starts_with("/nix/store"));
        if nix_gtk {
            // Discover only at build time. System GTK modules may use a
            // different ABI, so do not use them with a Nix GTK installation.
            let paths = nix_module_paths(Path::new("/nix/store"));
            if paths.is_empty() {
                println!("cargo:warning=Nix GTK4 was detected but no fcitx GTK4 module was found; build with the flake dev shell or set FCITX5_GTK_PATH to the module's lib/gtk-4.0 directory");
            } else if paths.len() > 1 {
                println!("cargo:warning=Multiple Nix fcitx GTK4 modules were found; set FCITX5_GTK_PATH explicitly to select the matching GTK installation");
            }
            first_module(paths)
        } else {
            first_module([
                "/run/current-system/sw/lib/gtk-4.0",
                "/usr/lib/gtk-4.0",
                "/usr/lib64/gtk-4.0",
                "/usr/lib/x86_64-linux-gnu/gtk-4.0",
                "/usr/local/lib/gtk-4.0",
            ].map(PathBuf::from))
        }
    });
    if let Some(path) = path {
        if let Some(path) = path.to_str().filter(|path| !path.contains(['\r', '\n'])) {
            println!("cargo:rustc-env=FCITX5_GTK_PATH={path}");
            println!("cargo:rerun-if-changed={path}");
        } else {
            println!("cargo:warning=The discovered fcitx GTK4 module path contains non-UTF8 or newline characters and cannot be embedded; provide a UTF8 FCITX5_GTK_PATH without newlines");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_requires_a_real_module_and_preserves_path_precedence() {
        let root = env::temp_dir().join(format!(
            "gtk-ime-build-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = root.join("first/lib/gtk-4.0");
        let second = root.join("second/lib/gtk-4.0");
        for path in [&first, &second] {
            fs::create_dir_all(path.join("4.0.0/immodules")).unwrap();
            fs::write(path.join("4.0.0/immodules/libim-fcitx5.so"), []).unwrap();
        }
        assert_eq!(
            first_module([root.join("missing"), second.clone(), first]),
            Some(second)
        );
        let store_module = root.join("hash-fcitx5-gtk-5.1.6/lib/gtk-4.0");
        fs::create_dir_all(store_module.join("4.0.0/immodules")).unwrap();
        fs::write(store_module.join("4.0.0/immodules/libim-fcitx5.so"), []).unwrap();
        fs::create_dir_all(root.join("hash-fcitx5-gtk-5.1.6-dev")).unwrap();
        assert_eq!(nix_module_paths(&root), vec![store_module]);
        fs::remove_dir_all(root).unwrap();
    }
}
