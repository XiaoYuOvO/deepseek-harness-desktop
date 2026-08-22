use crate::config;
use crate::service::cli;
use crate::service::download::{self, Installable};
use crate::service::plugin;
use crate::service::workflow;
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;

/// 按当前设置同步命令行集成（shim + PATH 注册）。
///
/// 安装/更新流程的收尾步骤，失败只记日志、不阻断主流程。
fn sync_cli_link(app_handle: &AppHandle) {
    let setting = config::get_store_dat_setting(app_handle);
    let result = if setting.cli_link_enabled {
        cli::ensure(app_handle)
    } else {
        cli::remove(app_handle)
    };
    if let Err(e) = result {
        log::warn!("cli link sync failed: {e}");
    }
}

/// 一键安装依赖（Node.js 运行时 + 打包的 Harness 发行版）
///
/// 启动逻辑由前端显式调用 `launch_harness` 完成，避免重复拉起进程。
#[tauri::command]
pub async fn install_dependencies(app_handle: AppHandle) -> Result<(), String> {
    if workflow::status::get_status() == workflow::status::Status::Installing {
        log::info!("Installation process already running, skipping");
        return Ok(());
    }

    // 以实际安装状态为准：本地安装与 GitHub 最新 release 的 commit hash
    // 不一致时，说明上游 pkg 有更新/修复，需要自动重新下载。
    let node_ok = download::Nodejs.check_installed(&app_handle);
    let dsh_files_ok = download::Dsh.check_installed(&app_handle);
    let dsh_latest = download::fetch_latest_dsh_pkg_info().await;

    let dsh_ok = match &dsh_latest {
        Ok(latest) => {
            dsh_files_ok
                && config::get_dsh_pkg_commit(&app_handle).as_deref()
                    == Some(latest.commit.as_str())
        }
        Err(e) => {
            // 网络不可用或 GitHub API 限流时保留本地安装，不阻塞启动
            log::warn!(
                "Failed to check latest dsh release info, keeping local install: {}",
                e
            );
            dsh_files_ok
        }
    };

    // pnpm 是 dsh plugin 子命令的运行时依赖（v0.3.0 起随环境安装）；老版本
    // 升级后 `installed` 已为 true 会跳过环境安装，捆绑 pnpm 可能从未落盘，
    // 需一并纳入"已就绪"判定，缺失时由 workflow::install 按任务补齐。
    let pnpm_ok = download::Pnpm.check_installed(&app_handle);

    if node_ok && dsh_ok && pnpm_ok {
        log::debug!("Dependencies already installed and up to date, skipping installation");
        let mut setting = config::get_store_dat_setting(&app_handle);
        if !setting.installed {
            setting.installed = true;
            config::set_store_dat_setting(&app_handle, setting);
        }
        sync_cli_link(&app_handle);
        return Ok(());
    }

    log::debug!("Dependencies missing or outdated, starting installation process");
    workflow::status::set_status(workflow::status::Status::Installing);
    workflow::status::emit_status(&app_handle);
    workflow::install(&app_handle, dsh_latest.ok()).await?;
    log::debug!("Installation completed, marked as installed");
    let mut setting = config::get_store_dat_setting(&app_handle);
    setting.installed = true;
    config::set_store_dat_setting(&app_handle, setting);
    sync_cli_link(&app_handle);
    Ok(())
}

/// 静默检查是否有新版 Harness 可用（只查不装，供进入页面后后台调用）
///
/// 以“实际安装文件”为准核对，而不是只看本地记录：记录可能因安装时 API
/// 失败或外围途径更新而滞后于文件，此时修正记录并免打扰；同版本热修
/// （版本相同但 commit 不同）仍正常提示。
#[tauri::command]
pub async fn check_dsh_update(
    app_handle: AppHandle,
) -> Result<Option<download::LatestDshPkg>, String> {
    // 本地没有安装时无需提示更新
    let dsh_files_ok = download::Dsh.check_installed(&app_handle);
    if !dsh_files_ok {
        return Ok(None);
    }

    let latest = download::fetch_latest_dsh_pkg_info().await?;
    let record_commit = config::get_dsh_pkg_commit(&app_handle);
    let record_tag = config::get_dsh_pkg_tag(&app_handle);
    let installed_version = config::get_dsh_version(&app_handle);

    // 老记录没有 tag，反查 pkg 仓库 tags 列表确认记录对应的发布版本；
    // 反查失败时由 resolve_update 回退到“以实际文件为准”的保守分支
    let legacy_tags = if record_tag.is_none() {
        download::fetch_dsh_pkg_tags().await.unwrap_or_default()
    } else {
        Vec::new()
    };

    match download::resolve_update(
        record_commit.as_deref(),
        record_tag.as_deref(),
        installed_version.as_deref(),
        &latest,
        &legacy_tags,
    ) {
        download::UpdateCheck::UpToDate => Ok(None),
        download::UpdateCheck::UpdateAvailable => Ok(Some(latest)),
        download::UpdateCheck::HealUpToDate => {
            // 安装文件已是最新 release，只是记录滞后：修正记录后下次启动
            // 直接走 commit 比对快速路径，不再误报
            log::info!(
                "Installed Harness files already at latest release, healing stale record: {} ({})",
                latest.tag,
                latest.commit
            );
            config::set_dsh_pkg_commit(&app_handle, latest.commit.clone());
            config::set_dsh_pkg_tag(&app_handle, latest.tag.clone());
            Ok(None)
        }
    }
}

/// 启动 Harness 服务
#[tauri::command]
pub async fn launch_harness(app_handle: AppHandle) -> Result<(), String> {
    workflow::launch(app_handle).await
}

/// 停止 Harness 服务
#[tauri::command]
pub async fn shutdown_harness(app_handle: AppHandle) -> Result<(), String> {
    workflow::stop(app_handle).await
}

/// 重启 Harness 服务
#[tauri::command]
pub async fn restart_harness(app_handle: AppHandle) -> Result<(), String> {
    workflow::restart(app_handle).await
}

/// 获取当前 Harness 服务状态
#[tauri::command]
pub fn get_dsh_status() -> workflow::status::Status {
    workflow::status::get_status()
}

/// 获取预装插件列表（含已安装检测结果），首次启动引导界面渲染用
#[tauri::command]
pub async fn get_preinstall_plugins(
    app_handle: AppHandle,
) -> Result<Vec<plugin::PreinstallPlugin>, String> {
    Ok(plugin::list(&app_handle))
}

/// 安装选中的预装插件（`dsh plugin --profile web add <ids...>`），
/// 进程输出实时通过 `preinstall-log` 事件推送；成功后标记引导完成。
#[tauri::command]
pub async fn install_preinstall_plugins(
    app_handle: AppHandle,
    ids: Vec<String>,
) -> Result<(), String> {
    plugin::install(&app_handle, &ids).await?;
    let mut setting = config::get_store_dat_setting(&app_handle);
    setting.preinstall_done = true;
    config::set_store_dat_setting(&app_handle, setting);
    Ok(())
}

/// 取消正在进行的预装插件安装（网络抖动/限流卡住时用户点“取消”）。
#[tauri::command]
pub async fn cancel_preinstall_plugins(app_handle: AppHandle) {
    plugin::cancel(&app_handle).await;
}

/// 跳过预装插件引导：仅记录状态，不再弹出
#[tauri::command]
pub async fn skip_preinstall_plugins(app_handle: AppHandle) -> Result<(), String> {
    let mut setting = config::get_store_dat_setting(&app_handle);
    setting.preinstall_done = true;
    config::set_store_dat_setting(&app_handle, setting);
    Ok(())
}

/// 在系统浏览器中打开预装插件的仓库地址（仅允许预装清单内的 id）
#[tauri::command]
pub async fn open_preinstall_repo(app_handle: AppHandle, id: String) -> Result<(), String> {
    let url = plugin::repo_url_of(&app_handle, &id)
        .ok_or_else(|| format!("PREINSTALL_INVALID_ID: {id}"))?;
    app_handle
        .opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 健康检查（通过 Rust 代理，避免 WebView CORS 问题）
#[tauri::command]
pub async fn proxy_health_check(app_handle: AppHandle) -> Result<String, String> {
    let port = config::get_store_dat_setting(&app_handle).port;
    workflow::proxy_health_check(port).await
}

/// 运行时/版本/诊断信息（侧边栏展示）
#[tauri::command]
pub async fn get_runtime_info(app_handle: AppHandle) -> Result<config::RuntimeInfo, String> {
    let port = config::get_store_dat_setting(&app_handle).port;
    Ok(config::runtime_info(&app_handle, port))
}

/// 当前桌面端配置
#[tauri::command]
pub async fn get_app_config(app_handle: AppHandle) -> Result<config::Setting, String> {
    Ok(config::get_store_dat_setting(&app_handle))
}

/// 更新桌面端配置
#[tauri::command]
pub async fn update_app_config(
    app_handle: AppHandle,
    port: Option<u16>,
    auto_start: Option<bool>,
    http_proxy: Option<String>,
    dsh_environment: Option<Vec<config::DshEnvironmentVariable>>,
    dsh_arguments: Option<Vec<String>>,
    cli_link_enabled: Option<bool>,
) -> Result<config::Setting, String> {
    let mut setting = config::get_store_dat_setting(&app_handle);
    if let Some(port) = port {
        if port == 0 {
            return Err("port must be a positive number".to_string());
        }
        setting.port = port;
    }
    if let Some(auto_start) = auto_start {
        setting.auto_start = auto_start;
    }
    if let Some(http_proxy) = http_proxy {
        setting.http_proxy = config::normalize_http_proxy(&http_proxy)?;
    }
    if let Some(environment) = dsh_environment {
        config::validate_dsh_environment(&environment)?;
        setting.dsh_environment = environment;
    }
    if let Some(arguments) = dsh_arguments {
        config::validate_dsh_arguments(&arguments)?;
        setting.dsh_arguments = arguments;
    }
    // 命令行集成：先执行文件系统/PATH 操作，成功后再持久化开关，
    // 失败时配置保持不变，避免"开关已开但 shim 未生成"的不一致状态。
    if let Some(enabled) = cli_link_enabled {
        if enabled {
            cli::ensure(&app_handle)?;
        } else {
            cli::remove(&app_handle)?;
        }
        setting.cli_link_enabled = enabled;
    }
    config::set_store_dat_setting(&app_handle, setting.clone());
    Ok(setting)
}

/// 命令行集成状态（shim 文件与 PATH 注册情况）
#[tauri::command]
pub fn get_cli_link_status(app_handle: AppHandle) -> Result<cli::CliLinkStatus, String> {
    Ok(cli::get_status(&app_handle))
}

/// 在系统浏览器中打开 Harness 界面
#[tauri::command]
pub async fn open_in_browser(app_handle: AppHandle) -> Result<(), String> {
    let url = config::get_dsh_service_url(config::get_store_dat_setting(&app_handle).port);
    app_handle
        .opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 复制 Harness 服务地址到剪贴板
#[tauri::command]
pub async fn copy_service_url(app_handle: AppHandle) -> Result<(), String> {
    let url = config::get_dsh_service_url(config::get_store_dat_setting(&app_handle).port);
    app_handle
        .clipboard()
        .write_text(url)
        .map_err(|e| e.to_string())
}

/// 将任意文本写入系统剪贴板。
///
/// WebView2 下内嵌 iframe 的 `navigator.clipboard.writeText` 权限受限
/// （跨源 iframe 不自动授予 clipboard-write），由客户端桥把复制请求
/// 转发到这里走原生剪贴板，保证 dsh 页面内“复制”按钮可用。
#[tauri::command]
pub async fn write_system_clipboard(app_handle: AppHandle, text: String) -> Result<(), String> {
    app_handle
        .clipboard()
        .write_text(text)
        .map_err(|e| format!("CLIPBOARD_WRITE_FAILED: {e}"))
}

/// 记录窗口拖拽桥注入失败（仅失败时上报，正常路径保持静默）。
///
/// 客户端插件在 dsh 会话 Header 挂载两个 slot 后发送 ready 心跳；
/// 壳侧 watchdog 超时未收到心跳时调用本命令，把失败原因写入服务日志，
/// 供用户在日志面板中排查（不弹 UI，只落日志）。
#[tauri::command]
pub fn report_window_drag_injection_failure(
    app_handle: AppHandle,
    detail: String,
) -> Result<(), String> {
    let detail = detail.replace(['\r', '\n'], " ");
    let entry = format!("[desktop-window-drag] injection failed: {detail}");
    log::error!("{entry}");

    let log_path = config::get_service_log_path(&app_handle);
    if let Some(parent) = log_path.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            use std::io::Write;
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .and_then(|mut file| file.write_all(format!("{entry}\n").as_bytes()));
        }
    }
    Ok(())
}

/// 在系统文件管理器中定位指定文件（Session 日志下载完成后的"在文件夹中显示"）
#[tauri::command]
pub fn reveal_in_folder(path: String) -> Result<(), String> {
    tauri_plugin_opener::reveal_item_in_dir(&path)
        .map_err(|e| format!("REVEAL_FAILED: {e}"))
}

/// 在系统文件管理器中打开数据目录
#[tauri::command]
pub async fn reveal_data_dir(app_handle: AppHandle) -> Result<(), String> {
    let app_data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?;

    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(&app_data_dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&app_data_dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&app_data_dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 读取 dsh 服务日志
#[tauri::command]
pub async fn read_service_logs(
    app_handle: AppHandle,
    max_bytes: Option<usize>,
) -> Result<String, String> {
    let log_path = config::get_service_log_path(&app_handle);
    if !log_path.exists() {
        return Ok(String::new());
    }

    let content = std::fs::read_to_string(&log_path).map_err(|e| e.to_string())?;
    let max_bytes = max_bytes.unwrap_or(64 * 1024);
    if content.len() <= max_bytes {
        Ok(content)
    } else {
        Ok(content[content.len() - max_bytes..].to_string())
    }
}

/// 清空 dsh 服务日志
#[tauri::command]
pub async fn clear_service_logs(app_handle: AppHandle) -> Result<(), String> {
    let log_path = config::get_service_log_path(&app_handle);
    std::fs::write(&log_path, "").map_err(|e| e.to_string())
}

/// 保存界面语言偏好
#[tauri::command]
pub fn set_language(app_handle: AppHandle, lang: String) {
    let mut setting = config::get_store_dat_setting(&app_handle);
    setting.language = lang.clone();
    config::set_store_dat_setting(&app_handle, setting);
    config::i18n::set_language(match lang.as_str() {
        "en" | "en-US" => config::i18n::Lang::En,
        _ => config::i18n::Lang::Zh,
    });
}

/// 当前 dsh 主题偏好（light/dark/system），用于让桌面外壳跟随内嵌页面主题
#[tauri::command]
pub fn get_dsh_theme(app_handle: AppHandle) -> config::DshTheme {
    config::get_dsh_theme(&app_handle)
}
