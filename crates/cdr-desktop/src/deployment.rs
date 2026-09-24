use std::fs;
use std::path::{Path, PathBuf};

const BUNDLE_FILES: &[&str] = &[
    "cdr-corel.exe",
    "opencv_world4140.dll",
    "concrt140.dll",
    "msvcp140.dll",
    "msvcp140_1.dll",
    "msvcp140_2.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "CdrOutline.CorelExtension",
];

#[derive(Debug, Clone)]
pub struct BundleStatus {
    pub missing_files: Vec<String>,
}

impl BundleStatus {
    pub fn is_ready(&self) -> bool {
        self.missing_files.is_empty()
    }

    pub fn processing_ready(&self) -> bool {
        self.missing_files
            .iter()
            .all(|name| name == "CdrOutline.CorelExtension")
    }
}

pub fn inspect_bundle(directory: &Path) -> BundleStatus {
    let missing_files = BUNDLE_FILES
        .iter()
        .filter(|name| !directory.join(name).is_file())
        .map(|name| (*name).to_owned())
        .collect();
    BundleStatus { missing_files }
}

pub fn discover_corel_installation() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from(r"D:\apps\CorelDRAW Graphics Suite 2020"),
        PathBuf::from(r"C:\Program Files\Corel\CorelDRAW Graphics Suite 2020"),
        PathBuf::from(r"C:\Program Files (x86)\Corel\CorelDRAW Graphics Suite 2020"),
    ];
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(variable) {
            let root = PathBuf::from(root);
            candidates.extend([
                root.join(r"Corel\CorelDRAW Graphics Suite 2020"),
                root.join(r"Corel\CorelDRAW Graphics Suite 2021"),
                root.join(r"Corel\CorelDRAW Graphics Suite 2022"),
                root.join(r"Corel\CorelDRAW Graphics Suite 2023"),
                root.join(r"Corel\CorelDRAW Graphics Suite 2024"),
                root.join(r"Corel\CorelDRAW Graphics Suite 2025"),
            ]);
        }
    }
    candidates
        .into_iter()
        .find(|path| is_corel_installation(path))
}

pub fn is_corel_installation(path: &Path) -> bool {
    path.join(r"Programs64\Assemblies\Corel.Interop.VGCore.dll")
        .is_file()
}

pub fn has_installed_extension(corel_root: &Path) -> bool {
    corel_root
        .join(r"Extensions\CdrOutline.CorelExtension")
        .is_file()
}

pub fn install_extension(bundle: &Path, corel_root: &Path) -> Result<String, String> {
    validate_corel_root(corel_root)?;
    let source = bundle.join("CdrOutline.CorelExtension");
    if !source.is_file() {
        return Err(format!("安装包中找不到插件：{}", source.display()));
    }

    let extensions = corel_root.join("Extensions");
    fs::create_dir_all(&extensions).map_err(|error| format!("无法创建 Corel 扩展目录：{error}"))?;
    let destination = extensions.join("CdrOutline.CorelExtension");
    if destination.is_dir() {
        return Err(format!(
            "目标位置存在同名目录，未做覆盖：{}",
            destination.display()
        ));
    }

    let nonce = format!("{}-{}", timestamp(), std::process::id());
    let staging = extensions.join(format!(".CdrOutline.CorelExtension.installing-{nonce}"));
    if let Err(error) = fs::copy(&source, &staging) {
        let _ = fs::remove_file(&staging);
        return Err(format!("复制插件失败：{error}"));
    }

    let backup = if destination.exists() {
        let backup_root = extensions.join("_CdrOutlineBackups").join(&nonce);
        if let Err(error) = fs::create_dir_all(&backup_root) {
            let _ = fs::remove_file(&staging);
            return Err(format!("无法创建插件备份目录：{error}"));
        }
        let backup_path = backup_root.join("CdrOutline.CorelExtension");
        if let Err(error) = fs::rename(&destination, &backup_path) {
            let _ = fs::remove_file(&staging);
            return Err(format!("无法备份已安装插件；原文件未覆盖：{error}"));
        }
        Some(backup_path)
    } else {
        None
    };

    if let Err(error) = fs::rename(&staging, &destination) {
        if let Some(backup_path) = &backup {
            let _ = fs::rename(backup_path, &destination);
        }
        let _ = fs::remove_file(&staging);
        return Err(format!("无法将插件放入 Corel 扩展目录：{error}"));
    }

    let backup_message = backup
        .map(|path| format!("旧版本已备份到 {}。", path.display()))
        .unwrap_or_default();
    Ok(format!(
        "插件已安装到 {}。请重启 CorelDRAW 后使用。{}",
        destination.display(),
        backup_message
    ))
}

pub fn uninstall_extension(corel_root: &Path) -> Result<String, String> {
    validate_corel_root(corel_root)?;
    let extensions = corel_root.join("Extensions");
    let installed = extensions.join("CdrOutline.CorelExtension");
    if !installed.is_file() {
        return Err(format!("没有找到已安装的插件文件：{}", installed.display()));
    }

    let backup = extensions
        .join("_CdrOutlineBackups")
        .join(format!("{}-{}", timestamp(), std::process::id()))
        .join("CdrOutline.CorelExtension");
    let backup_parent = backup
        .parent()
        .ok_or_else(|| "无法确定插件备份目录".to_owned())?;
    fs::create_dir_all(backup_parent)
        .map_err(|error| format!("无法创建可恢复备份目录：{error}"))?;
    fs::rename(&installed, &backup).map_err(|error| format!("卸载插件失败：{error}"))?;
    Ok(format!(
        "插件已从 Corel 扩展目录移出，并保存在 {}。请重启 CorelDRAW。",
        backup.display()
    ))
}

fn validate_corel_root(path: &Path) -> Result<(), String> {
    if !is_corel_installation(path) {
        return Err(format!(
            "此目录不像 CorelDRAW 安装目录（缺少 Programs64\\Assemblies\\Corel.Interop.VGCore.dll）：{}",
            path.display()
        ));
    }
    Ok(())
}

fn timestamp() -> String {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{}", duration.as_secs(), duration.subsec_nanos())
}
