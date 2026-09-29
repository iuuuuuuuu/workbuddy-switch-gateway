//! 构建脚本：把网关可执行文件压缩后内嵌，实现单文件分发。
//!
//! 查找顺序（任一命中即内嵌）：
//!   1. 环境变量 WB_SWITCH_GATEWAY_BIN
//!   2. crates/wb-switch-core/embedded/gateway[.exe]（约定目录）
//!   3. 仓库根 dist/gateway[.exe]
//!   4. 前三者都没有时，现场调用 `go build` 从 go-gateway/ 编译一份
//!
//! 全部失败时生成 `None`，程序仍可编译，只是不内嵌网关
//!（此时回退到用户自备 gateway 可执行文件的旧方式）。
//!
//! 为什么要加第 4 步：此前只做「找不到就静默跳过」，而
//! crates/wb-switch-core/embedded/ 在 .gitignore 里，本地直接
//! `cargo build` / `npm run tauri build` 时若忘了先跑
//! scripts/build-gateway，产物里就没有网关 —— 运行时报
//! 「未找到网关可执行文件」，而构建期毫无提示，极难定位。
//! 现场编译让「忘了先构建网关」不再是一个坑。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("WB_SWITCH_GATEWAY_BIN") {
        if !p.trim().is_empty() {
            out.push(PathBuf::from(p));
        }
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    // Windows 产物带 .exe 后缀；macOS/Linux 不带。与 scripts/build-gateway.sh 一致。
    let name = if cfg!(windows) { "gateway.exe" } else { "gateway" };
    out.push(manifest.join("embedded").join(name));
    // 仓库根 dist/（crates/wb-switch-core → ../..）
    if let Some(root) = manifest.parent().and_then(Path::parent) {
        out.push(root.join("dist").join(name));
    }
    out
}

/// 仓库根目录（crates/wb-switch-core → ../..）。
fn repo_root() -> Option<PathBuf> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    manifest.parent()?.parent().map(Path::to_path_buf)
}

/// 目标平台的 GOOS / GOARCH。用 CARGO_CFG_TARGET_* 而非宿主信息，
/// 交叉编译（如 Linux 上编 Windows 产物）时才不会取错平台。
fn go_target() -> (String, String) {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let goos = match os.as_str() {
        "windows" => "windows",
        "macos" => "darwin",
        _ => "linux",
    };
    let goarch = match arch.as_str() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        _ => "amd64",
    };
    (goos.to_string(), goarch.to_string())
}

/// 从 go-gateway/ 现场编译网关，产物落在 out_dir（不污染仓库工作区）。
fn build_gateway_from_source(out_dir: &Path) -> Option<PathBuf> {
    let src = repo_root()?.join("go-gateway");
    if !src.join("cmd").join("server").is_dir() {
        return None;
    }
    let (goos, goarch) = go_target();
    let out = out_dir.join("gateway-go-build");
    let result = Command::new("go")
        .arg("build")
        .arg("-trimpath")
        .arg("-ldflags")
        .arg("-s -w")
        .arg("-o")
        .arg(&out)
        .arg("./cmd/server")
        .current_dir(&src)
        // CGO_ENABLED=0：纯静态链接，交叉编译时不需要目标平台 C 工具链。
        .env("CGO_ENABLED", "0")
        .env("GOOS", &goos)
        .env("GOARCH", &goarch)
        .env("GOFLAGS", "-mod=mod")
        .status();
    match result {
        Ok(s) if s.success() && out.is_file() => {
            println!("cargo:warning=已从 go-gateway/ 现场编译网关（{goos}/{goarch}）");
            Some(out)
        }
        Ok(s) => {
            println!("cargo:warning=go build 失败（退出码 {s}），本次构建不内嵌网关");
            None
        }
        Err(e) => {
            println!("cargo:warning=未能执行 go build（{e}），本次构建不内嵌网关");
            None
        }
    }
}

/// 递归登记 go 源文件为 rerun-if-changed：网关代码改动后自动重新编译内嵌。
fn watch_go_sources(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            watch_go_sources(&path);
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("go")) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=WB_SWITCH_GATEWAY_BIN");
    // 无条件注册所有候选路径的监听：这样「先构建时没有网关、后来补上」
    // 也能触发 build.rs 重跑。只在命中路径上注册会导致永远内嵌不进去。
    for c in candidate_paths() {
        println!("cargo:rerun-if-changed={}", c.display());
    }
    if let Some(root) = repo_root() {
        let gw = root.join("go-gateway");
        for extra in ["go.mod", "go.sum"] {
            println!("cargo:rerun-if-changed={}", gw.join(extra).display());
        }
        watch_go_sources(&gw.join("cmd"));
        watch_go_sources(&gw.join("internal"));
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let gen_file = out_dir.join("gateway_embed.rs");

    // 1~3：预置产物优先（环境变量 / embedded/ / dist/）
    let mut chosen: Option<PathBuf> = None;
    for c in candidate_paths() {
        if c.is_file() {
            if let Ok(meta) = std::fs::metadata(&c) {
                if meta.len() > 1024 * 1024 {
                    chosen = Some(c);
                    break;
                }
            }
        }
    }

    // 4：都没有 → 现场编译
    let chosen = chosen.or_else(|| build_gateway_from_source(&out_dir));

    let Some(path) = chosen else {
        std::fs::write(
            &gen_file,
            "// 未内嵌网关二进制（构建时未找到 gateway 可执行文件）\nNone::<&[u8]>",
        )
        .expect("写入 gateway_embed.rs 失败");
        println!("cargo:warning=未找到网关二进制，本次构建不内嵌（可设 WB_SWITCH_GATEWAY_BIN 指定）");
        return;
    };

    println!("cargo:rerun-if-changed={}", path.display());
    let raw = std::fs::read(&path).expect("读取网关二进制失败");

    // gzip 压缩：体积可降约 60%
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&raw).expect("压缩网关失败");
    let gz = enc.finish().expect("压缩网关失败");

    std::fs::write(
        &gen_file,
        format!(
            "// 由 build.rs 生成：内嵌网关（原始 {} 字节 → 压缩 {} 字节）\nSome(&{:?})",
            raw.len(),
            gz.len(),
            gz
        ),
    )
    .expect("写入 gateway_embed.rs 失败");

    println!(
        "cargo:warning=已内嵌网关: {} ({} MB → {} MB)，仅需分发单个可执行文件",
        path.display(),
        raw.len() / 1048576,
        gz.len() / 1048576
    );
}