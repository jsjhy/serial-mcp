//! 将原生应用清单嵌入 EXE：启用 Windows 11 系统控件样式、系统 DPI 感知和长路径。
//! 资源编译只在 Windows GUI 构建执行，不影响旧 TUI/MCP；不下载构建工具。
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=gui.manifest");
    if env::var_os("CARGO_FEATURE_GUI").is_none()
        || !env::var("TARGET").unwrap().contains("windows")
    {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("gui.manifest");
    let source = out.join("gui.rc");
    fs::write(
        &source,
        format!(
            "1 24 \"{}\"\n",
            manifest.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    let gnu = env::var("TARGET").unwrap().ends_with("gnu");
    let object = out.join(if gnu {
        "gui-resource.o"
    } else {
        "gui-resource.res"
    });
    let mut compiler = Command::new(if gnu { "windres" } else { "rc.exe" });
    if gnu {
        compiler
            .arg("-i")
            .arg(&source)
            .arg("-o")
            .arg(&object)
            .args(["-O", "coff"]);
    } else {
        compiler
            .arg("/nologo")
            .arg(format!("/fo{}", object.display()))
            .arg(&source);
    }
    assert!(
        compiler
            .status()
            .expect("Windows resource compiler is required (windres or Windows SDK rc.exe)")
            .success(),
        "manifest compilation failed"
    );
    println!("cargo:rustc-link-arg={}", object.display());
}
