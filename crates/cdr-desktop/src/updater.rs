use reqwest::Proxy;
use reqwest::blocking::{Client, ClientBuilder};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const RELEASES_URL: &str =
    "https://api.github.com/repos/BaiLiAiChiBiJiLin/CDR-ImageOutline/releases/latest";

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    zip_url: String,
    checksum: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn check_latest() -> Result<Option<UpdateInfo>, String> {
    let client = client_builder()
        .user_agent(format!("CDR-ImageOutline/{}", current_version()))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("创建更新连接失败：{e}"))?;
    let release: Release = client
        .get(RELEASES_URL)
        .send()
        .map_err(|e| format!("检查更新失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub 更新服务返回错误：{e}"))?
        .json()
        .map_err(|e| format!("读取更新信息失败：{e}"))?;
    let version = release.tag_name.trim_start_matches('v').to_owned();
    if !is_newer(&version, current_version()) {
        return Ok(None);
    }
    let zip = release
        .assets
        .iter()
        .find(|a| a.name.ends_with("-Windows-x64.zip"))
        .ok_or("最新版本没有 Windows x64 更新包")?;
    let checksum_asset = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.txt")
        .ok_or("最新版本没有 SHA-256 校验文件")?;
    let checksums = client
        .get(&checksum_asset.browser_download_url)
        .send()
        .map_err(|e| format!("下载校验文件失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("校验文件返回错误：{e}"))?
        .text()
        .map_err(|e| format!("读取校验文件失败：{e}"))?;
    let checksum = checksums
        .lines()
        .find(|line| line.contains(&zip.name))
        .and_then(|line| line.split_whitespace().next())
        .filter(|value| value.len() == 64)
        .ok_or("校验文件中没有更新包的 SHA-256")?
        .to_owned();
    Ok(Some(UpdateInfo {
        version,
        zip_url: zip.browser_download_url.clone(),
        checksum,
    }))
}

pub fn download_and_install(info: &UpdateInfo, bundle_dir: &Path) -> Result<(), String> {
    let temp = std::env::temp_dir().join(format!("cdr-update-{}", std::process::id()));
    fs::create_dir_all(&temp).map_err(|e| format!("创建更新临时目录失败：{e}"))?;
    let zip_path = temp.join("update.zip");
    let mut response = client_builder()
        .user_agent(format!("CDR-ImageOutline/{}", current_version()))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("创建下载连接失败：{e}"))?
        .get(&info.zip_url)
        .send()
        .map_err(|e| format!("下载更新失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("更新下载返回错误：{e}"))?;
    let mut file = File::create(&zip_path).map_err(|e| format!("保存更新包失败：{e}"))?;
    response
        .copy_to(&mut file)
        .map_err(|e| format!("写入更新包失败：{e}"))?;
    file.flush().map_err(|e| format!("刷新更新包失败：{e}"))?;
    let mut hasher = Sha256::new();
    let mut verify = File::open(&zip_path).map_err(|e| format!("打开更新包失败：{e}"))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = verify
            .read(&mut buffer)
            .map_err(|e| format!("读取更新包失败：{e}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != info.checksum.to_ascii_lowercase() {
        return Err("更新包 SHA-256 校验失败，已停止更新。".to_owned());
    }
    let script = temp.join("apply-update.ps1");
    let script_text = format!(
        r#"$ErrorActionPreference = 'Stop'
$pidToWait = {pid}
Wait-Process -Id $pidToWait
$stage = Join-Path $env:TEMP 'cdr-update-stage-{pid}'
Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
Expand-Archive -LiteralPath '{zip}' -DestinationPath $stage -Force
Get-ChildItem -LiteralPath $stage -File -Recurse | ForEach-Object {{
  $relative = $_.FullName.Substring($stage.Length).TrimStart('\')
  $destination = Join-Path '{bundle}' $relative
  New-Item -ItemType Directory -Path (Split-Path $destination) -Force | Out-Null
  Copy-Item -LiteralPath $_.FullName -Destination $destination -Force
}}
Remove-Item -LiteralPath $stage -Recurse -Force
Start-Process -FilePath '{exe}'
Remove-Item -LiteralPath '{script}' -Force
"#,
        pid = std::process::id(),
        zip = ps_quote(&zip_path),
        bundle = ps_quote(bundle_dir),
        exe = ps_quote(&bundle_dir.join("cdr-desktop.exe")),
        script = ps_quote(&script)
    );
    fs::write(&script, script_text).map_err(|e| format!("创建更新程序失败：{e}"))?;
    Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .spawn()
        .map_err(|e| format!("启动更新程序失败：{e}"))?;
    Ok(())
}

fn ps_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn client_builder() -> ClientBuilder {
    let builder = Client::builder();
    let Some(proxy) = system_proxy() else {
        return builder;
    };
    match Proxy::all(proxy) {
        Ok(proxy) => builder.proxy(proxy),
        Err(_) => builder,
    }
}

fn system_proxy() -> Option<String> {
    for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
    }
    #[cfg(windows)]
    {
        let output = Command::new("reg")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                "/v",
                "ProxyServer",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let value = text
            .lines()
            .find(|line| line.contains("ProxyServer"))
            .and_then(|line| line.split_whitespace().last())?;
        let value = value
            .split(';')
            .find_map(|part| {
                part.strip_prefix("https=")
                    .or_else(|| part.strip_prefix("http="))
            })
            .unwrap_or(value);
        return Some(
            if value.starts_with("http://") || value.starts_with("https://") {
                value.to_owned()
            } else {
                format!("http://{value}")
            },
        );
    }
    #[cfg(not(windows))]
    None
}

fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |value: &str| {
        value
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let mut left = parse(candidate);
    let mut right = parse(current);
    left.resize(3, 0);
    right.resize(3, 0);
    left > right
}

#[cfg(test)]
mod tests {
    use super::is_newer;
    #[test]
    fn compares_versions() {
        assert!(is_newer("0.1.25", "0.1.24"));
        assert!(!is_newer("0.1.24", "0.1.25"));
    }
}
