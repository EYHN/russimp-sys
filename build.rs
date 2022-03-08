mod build_support;

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use build_support::{static_lib_filename, Target};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};

const BINDINGS_FILE: &str = "bindings.rs";
const WRAPPER_FILE: &str = "wrapper.h";

fn run_bindgen(output_file: impl AsRef<Path>, include_path: Option<&Path>) -> Result<(), ()> {
    let mut builder = bindgen::Builder::default();
    if let Some(include_path) = include_path {
        builder = builder.clang_arg(format!("-I{}", include_path.to_str().unwrap()));
    }
    builder
        .header(WRAPPER_FILE)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        .allowlist_type("ai.*")
        .allowlist_function("ai.*")
        .allowlist_var("ai.*")
        .allowlist_var("AI_.*")
        .derive_partialeq(true)
        .derive_eq(true)
        .derive_hash(true)
        .derive_debug(true)
        .generate()?
        .write_to_file(output_file.as_ref())
        .unwrap();
    Ok(())
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct BuildManifest {
    pub link_search_dir: PathBuf,
    pub assimp_license: PathBuf,
    pub link_libs: Vec<String>,
    pub bindings_rs: PathBuf,
    pub target: String,
}

fn install(manifest: &BuildManifest) {
    println!(
        "cargo:rustc-link-search=native={}",
        manifest.link_search_dir.display().to_string()
    );

    for lib in &manifest.link_libs {
        println!("cargo:rustc-link-lib=static={}", lib);
    }

    let target = Target::parse_target(&manifest.target);

    if target.system == "linux" && cfg!(not(feature = "nolibcxx")) {
        println!("cargo:rustc-link-lib={}", "stdc++");
    }

    if target.system == "darwin" && cfg!(not(feature = "nolibcxx")) {
        println!("cargo:rustc-link-lib={}", "c++");
    }

    // Write the bindings to the <OUT_DIR>/bindings.rs file.
    let bindings_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join(BINDINGS_FILE);
    fs::copy(&manifest.bindings_rs, bindings_path).unwrap();
}

fn build_from_source(target: &Target) -> BuildManifest {
    let current_dir = env::current_dir().expect("Failed to get current dir");

    println!("cargo:rerun-if-env-changed=ASSIMP_SOURCE_DIR");
    // use <ASSIMP_SOURCE_DIR> or <current_dir>/assimp
    let assimp_source_dir = current_dir.join(
        env::var("ASSIMP_SOURCE_DIR")
            .unwrap_or(current_dir.join("assimp").to_str().unwrap().to_string()),
    );
    if assimp_source_dir.exists() == false {
        // source dir not exist, try to clone it
        let mut git_clone = Command::new("git");
        git_clone
            .arg("clone")
            .arg("https://github.com/assimp/assimp.git")
            .arg(&assimp_source_dir)
            .arg("--depth=1");
        build_support::run_command(&mut git_clone, "git");
    }

    println!("cargo:rerun-if-env-changed=RUSSIMP_BUILD_OUT_DIR");
    // use <RUSSIMP_BUILD_OUT_DIR> or <OUT_DIR>/build-from-source
    let out_dir = current_dir.join(
        env::var("RUSSIMP_BUILD_OUT_DIR").unwrap_or(
            PathBuf::from(env::var("OUT_DIR").unwrap())
                .join("build-from-source")
                .to_string_lossy()
                .to_string(),
        ),
    );

    let assimp_build_dir = out_dir.join("assimp");
    let assimp_install_dir = assimp_build_dir.join("out");
    let assimp_lib_dir = assimp_install_dir.join("lib");
    let assimp_include_dir = assimp_install_dir.join("include");
    let assimp_license = assimp_source_dir.join("LICENSE");
    let bindings_rs = out_dir.join("bindings.rs");

    // configure assimp
    fs::create_dir_all(&assimp_build_dir).unwrap();
    let mut assimp_cmake = Command::new("cmake");
    assimp_cmake
        .current_dir(&assimp_build_dir)
        .arg(&assimp_source_dir)
        .arg(format!("-DCMAKE_BUILD_TYPE={}", "Release"))
        .arg(format!(
            "-DCMAKE_INSTALL_PREFIX={}",
            assimp_install_dir.to_str().unwrap()
        ))
        .arg(format!("-DBUILD_SHARED_LIBS={}", "OFF"))
        .arg(format!("-DASSIMP_BUILD_ASSIMP_TOOLS={}", "OFF"))
        .arg(format!("-DASSIMP_BUILD_TESTS={}", "OFF"))
        .arg(format!(
            "-DASSIMP_BUILD_ZLIB={}",
            if cfg!(feature = "nozlib") {
                "OFF"
            } else {
                "ON"
            }
        ));

    if target.system == "windows" {
        // if windows
        if target.abi == Some("gnu".to_owned()) {
            panic!("MinGW is not supported");
        }

        match target.architecture.as_str() {
            "x86_64" => assimp_cmake.args(["-A", "x64"]),
            "i686" => assimp_cmake.args(["-A", "Win32"]),
            _ => panic!("Unsupported architecture"),
        };
    } else {
        // if not windows,  use ninja and clang
        assimp_cmake
            .env(
                "CMAKE_GENERATOR",
                env::var("CMAKE_GENERATOR").unwrap_or("Ninja".to_string()),
            )
            .env("CC", env::var("CC").unwrap_or("clang".to_string()))
            .env("CXX", env::var("CXX").unwrap_or("clang++".to_string()))
            .env("ASM", env::var("ASM").unwrap_or("clang".to_string()))
            .env(
                "CXXFLAGS",
                env::var("CXXFLAGS").unwrap_or(format!("-target {}", target.to_string())),
            )
            .env(
                "CFLAGS",
                env::var("CFLAGS").unwrap_or(format!("-target {}", target.to_string())),
            );
    }

    build_support::run_command(&mut assimp_cmake, "cmake");

    // build assimp
    let mut assimp_cmake_install = Command::new("cmake");
    assimp_cmake_install
        .current_dir(&assimp_build_dir)
        .args(["--build", "."])
        .args(["--target", "install"])
        .args(["--config", "Release"])
        .args([
            "--parallel",
            &env::var("NUM_JOBS").unwrap_or(num_cpus::get().to_string()),
        ]);

    build_support::run_command(&mut assimp_cmake_install, "cmake");

    let mut link_libs = if target.system == "windows" {
        // if windows, there is a suffix after the assimp library name, find library name here.
        let assimp_lib = fs::read_dir(&assimp_lib_dir)
            .unwrap()
            .map(|e| e.unwrap())
            .find(|f| f.file_name().to_string_lossy().starts_with("assimp"))
            .expect("Failed to find assimp library");
        vec![assimp_lib
            .file_name()
            .to_str()
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .to_owned()]
    } else {
        vec!["assimp".to_owned()]
    };

    if cfg!(not(feature = "nozlib")) {
        link_libs.push("zlibstatic".to_owned())
    }

    // generate bindings.rs
    run_bindgen(&bindings_rs, Some(&assimp_include_dir)).unwrap();

    BuildManifest {
        link_search_dir: assimp_lib_dir,
        assimp_license,
        link_libs,
        bindings_rs,
        target: target.to_string(),
    }
}

fn package(manifest: &BuildManifest, output: impl AsRef<Path>) {
    let file = fs::File::create(output).unwrap();
    let enc = GzEncoder::new(file, Compression::default());
    let mut tar_builder = tar::Builder::new(enc);

    tar_builder
        .append_file(
            "bindings.rs",
            &mut fs::File::open(&manifest.bindings_rs).unwrap(),
        )
        .unwrap();
    tar_builder
        .append_file(
            "LICENSE",
            &mut fs::File::open(&manifest.assimp_license).unwrap(),
        )
        .unwrap();

    tar_builder
        .append_dir("lib", &manifest.link_search_dir)
        .unwrap();

    for lib_name in &manifest.link_libs {
        let filename = static_lib_filename(&lib_name);
        tar_builder
            .append_file(
                format!("lib/{}", filename),
                &mut fs::File::open(&manifest.link_search_dir.join(filename)).unwrap(),
            )
            .unwrap();
    }

    let manifest_json = serde_json::to_string(&BuildManifest {
        link_search_dir: PathBuf::from("lib"),
        assimp_license: PathBuf::from("LICENSE"),
        link_libs: manifest.link_libs.clone(),
        bindings_rs: PathBuf::from("bindings.rs"),
        target: manifest.target.clone(),
    })
    .unwrap();
    let manifest_json_date = manifest_json.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_json_date.len() as u64);
    header.set_cksum();
    header.set_mode(0o644);
    header.set_mtime(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u64,
    );

    tar_builder
        .append_data(&mut header, "manifest.json", manifest_json_date)
        .unwrap();

    tar_builder.finish().unwrap();
}

fn download_from_cache(cache_tar_name: impl AsRef<str>, version: impl AsRef<str>) -> BuildManifest {
    let download_url = format!(
        "https://github.com/EYHN/russimp-sys/releases/download/v{}/{}",
        version.as_ref(),
        cache_tar_name.as_ref()
    );

    println!("Downloading {}", download_url);
    let package = build_support::download(cache_tar_name, download_url).expect("Download Failed");
    return unpack(&package);
}

fn unpack(package: impl AsRef<Path>) -> BuildManifest {
    let unpack_dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("unpack");
    fs::create_dir_all(&unpack_dir).unwrap();

    let file = fs::File::open(package).unwrap();
    let mut tar_archive = tar::Archive::new(GzDecoder::new(file));

    tar_archive.unpack(&unpack_dir).unwrap();

    let manifest_json = unpack_dir.join("manifest.json");
    let manifest: BuildManifest =
        serde_json::from_reader(io::BufReader::new(fs::File::open(manifest_json).unwrap()))
            .unwrap();

    BuildManifest {
        link_search_dir: unpack_dir.join(manifest.link_search_dir),
        assimp_license: unpack_dir.join(manifest.assimp_license),
        link_libs: manifest.link_libs.clone(),
        bindings_rs: unpack_dir.join(manifest.bindings_rs),
        target: manifest.target.clone(),
    }
}

fn main() {
    let target = build_support::Target::target();
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut feature_suffix = String::new();
    if cfg!(feature = "nozlib") {
        feature_suffix.push_str("-nozlib");
    }

    let cache_tar_name = format!(
        "russimp-{}-{}{}.tar.gz",
        version,
        target.to_string(),
        feature_suffix
    );

    println!("cargo:rerun-if-env-changed=RUSSIMP_PREBUILT");
    let use_cache = env::var("RUSSIMP_PREBUILT").unwrap_or("ON".to_string()) != "OFF"
        && cfg!(feature = "prebuilt");

    let build_manifest = if use_cache {
        download_from_cache(&cache_tar_name, &version)
    } else {
        let build_manifest = build_from_source(&target);

        // write build result to cache directory
        println!("cargo:rerun-if-env-changed=RUSSIMP_BUILD_CACHE_DIR");
        if let Ok(cache_dir) = env::var("RUSSIMP_BUILD_CACHE_DIR") {
            fs::create_dir_all(&cache_dir).unwrap();
            let output_tar_path = Path::new(&cache_dir).join(&cache_tar_name);
            package(&build_manifest, output_tar_path);
        }

        build_manifest
    };

    install(&build_manifest);
}
