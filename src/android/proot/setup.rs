use super::process::ArchProcess;
use crate::{
    android::{
        app::build::PolarBearBackend,
        backend::{
            wayland::{Compositor, WaylandBackend},
            webview::{ErrorVariant, WebviewBackend},
        },
        utils::application_context::get_application_context,
        utils::ndk::run_in_jvm,
    },
    core::{
        config::{CommandConfig, ARCH_FS_ARCHIVE, ARCH_FS_ROOT},
        logging::PolarBearExpectation,
    },
};
use jni::objects::JObject;
use jni::sys::_jobject;
use pathdiff::diff_paths;
use smithay::utils::Clock;
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{symlink, PermissionsExt},
    path::Path,
    sync::{
        mpsc::{self, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
};
use winit::platform::android::activity::AndroidApp;

#[derive(Debug)]
pub enum SetupMessage {
    Progress(String),
    Error(String),
}

pub struct SetupOptions {
    pub android_app: AndroidApp,
    pub mpsc_sender: Sender<SetupMessage>,
}

/// Setup is a process that should be done **only once** when the user installed the app.
/// The setup process consists of several stages.
/// Each stage is a function that takes the `SetupOptions` and returns a `StageOutput`.
type SetupStage = Box<dyn Fn(&SetupOptions) -> StageOutput + Send>;

/// Each stage should indicate whether the associated task is done previously or not.
/// Thus, it should return a finished status if the task is done, so that the setup process can move on to the next stage.
/// Otherwise, it should return a `JoinHandle`, so that the setup process can wait for the task to finish, but not block the main thread so that the setup progress can be reported to the user.
type StageOutput = Option<JoinHandle<()>>;

fn emit_setup_error(sender: &Sender<SetupMessage>, message: impl Into<String>) {
    let message = message.into();
    log::info!("Setup error: {}", message);
    sender.send(SetupMessage::Error(message)).unwrap_or(());
}

fn setup_arch_fs(options: &SetupOptions) -> StageOutput {
    let context = get_application_context();
    let temp_file = context.data_dir.join("ubuntu-fs.tar.gz");
    let local_file = Path::new("/data/local/tmp/linuxfs-stripped.tar.gz");
    let fs_root = Path::new(ARCH_FS_ROOT);
    let mpsc_sender = options.mpsc_sender.clone();

    // Only run if the fs_root is missing or empty
    // TODO: Setup integration test to make sure on clean install, the fs_root is either non existent or empty
    let need_setup = fs_root.read_dir().map_or(true, |mut d| d.next().is_none());
    if need_setup {
        return Some(thread::spawn(move || {
            // Check for local file first
            if local_file.exists() && !temp_file.exists() {
                mpsc_sender
                    .send(SetupMessage::Progress(
                        "Using local Ubuntu FS from /data/local/tmp...".to_string(),
                    ))
                    .pb_expect("Failed to send log message");

                if let Err(e) = fs::copy(local_file, &temp_file) {
                    emit_setup_error(
                        &mpsc_sender,
                        format!("Failed to copy local file: {}. Will try download...", e),
                    );
                } else {
                    log::info!("Copied local file from {:?} to {:?}", local_file, temp_file);
                }
            }

            // Download if the archive doesn't exist
            loop {
                if !temp_file.exists() {
                    mpsc_sender
                        .send(SetupMessage::Progress(
                            "Downloading Ubuntu FS...".to_string(),
                        ))
                        .pb_expect("Failed to send log message");

                    let response = match reqwest::blocking::get(ARCH_FS_ARCHIVE) {
                        Ok(response) => response,
                        Err(err) => {
                            emit_setup_error(
                                &mpsc_sender,
                                format!("Failed to download Ubuntu FS: {}. Retrying...", err),
                            );
                            continue;
                        }
                    };

                    let total_size = response.content_length().unwrap_or(0);
                    let mut file = match File::create(&temp_file) {
                        Ok(file) => file,
                        Err(err) => {
                            emit_setup_error(
                                &mpsc_sender,
                                format!(
                                    "Failed to create temp file for Arch Linux FS: {}. Retrying...",
                                    err
                                ),
                            );
                            continue;
                        }
                    };

                    let mut downloaded = 0u64;
                    let mut buffer = [0u8; 8192];
                    let mut reader = response;
                    let mut last_percent = 0;
                    let mut should_retry_download = false;

                    loop {
                        let n = match reader.read(&mut buffer) {
                            Ok(n) => n,
                            Err(err) => {
                                emit_setup_error(
                                    &mpsc_sender,
                                    format!("Failed to read from response: {}. Retrying...", err),
                                );
                                should_retry_download = true;
                                break;
                            }
                        };
                        if n == 0 {
                            break;
                        }
                        if let Err(err) = file.write_all(&buffer[..n]) {
                            emit_setup_error(
                                &mpsc_sender,
                                format!("Failed to write to file: {}. Retrying...", err),
                            );
                            should_retry_download = true;
                            break;
                        }
                        downloaded += n as u64;
                        if total_size > 0 {
                            let percent = (downloaded * 100 / total_size).min(100) as u8;
                            if percent != last_percent {
                                let downloaded_mb = downloaded as f64 / 1024.0 / 1024.0;
                                let total_mb = total_size as f64 / 1024.0 / 1024.0;
                                mpsc_sender
                                    .send(SetupMessage::Progress(format!(
                                        "Downloading Ubuntu FS... {}% ({:.2} MB / {:.2} MB)",
                                        percent, downloaded_mb, total_mb
                                    )))
                                    .unwrap_or(());
                                last_percent = percent;
                            }
                        }
                    }

                    if should_retry_download {
                        let _ = fs::remove_file(&temp_file);
                        continue;
                    }
                }

                mpsc_sender
                    .send(SetupMessage::Progress(
                        "Extracting Ubuntu FS...".to_string(),
                    ))
                    .pb_expect("Failed to send log message");

                // Ensure the target directory is clean and exists
                let _ = fs::remove_dir_all(fs_root);
                fs::create_dir_all(fs_root).pb_expect("Failed to create target directory");

                // Extract using proot + tar to avoid permission issues
                let temp_file_str = temp_file.to_str().unwrap();
                let fs_root_str = fs_root.to_str().unwrap();

                match ArchProcess::extract_rootfs(temp_file_str, fs_root_str) {
                    Ok(status) => {
                        log::info!(
                            "Rootfs extraction completed with status: {:?}",
                            status.code()
                        );
                        // Check if critical directories exist (tar may fail on absolute paths but most files extract)
                        if fs_root.join("usr").exists() && fs_root.join("bin").exists() {
                            log::info!("Rootfs appears valid despite tar exit code");
                        } else {
                            log::error!("Rootfs extraction appears incomplete");
                            let _ = fs::remove_dir_all(fs_root);
                            let _ = fs::remove_file(&temp_file);
                            emit_setup_error(
                                &mpsc_sender,
                                "Rootfs extraction incomplete".to_string(),
                            );
                            continue;
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to execute rootfs extraction: {}", e);
                        // Clean up the failed extraction
                        let _ = fs::remove_dir_all(fs_root);
                        let _ = fs::remove_file(&temp_file);
                        emit_setup_error(
                            &mpsc_sender,
                            format!("Failed to execute extraction command: {}", e),
                        );
                        continue;
                    }
                }

                // If we get here, extraction was successful
                break;
            }

            // Clean up the temporary file
            let _ = fs::remove_file(&temp_file);
        }));
    }
    None
}

fn simulate_linux_sysdata_stage(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let mpsc_sender = options.mpsc_sender.clone();

    if !fs_root.join("proc/.version").exists() {
        return Some(thread::spawn(move || {
            mpsc_sender
                .send(SetupMessage::Progress(
                    "Simulating Linux system data...".to_string(),
                ))
                .pb_expect(&format!("Failed to send log message"));

            log::info!("[simulate_linux_sysdata] Starting system data simulation");

            // Create necessary directories - don't fail if they already exist
            let proc_path = fs_root.join("proc");
            let sys_path = fs_root.join("sys");
            let sys_empty_path = fs_root.join("sys/.empty");

            log::info!(
                "[simulate_linux_sysdata] Creating directory: {:?}",
                proc_path
            );
            match fs::create_dir_all(&proc_path) {
                Ok(_) => log::info!(
                    "[simulate_linux_sysdata] Successfully created directory: {:?}",
                    proc_path
                ),
                Err(e) => log::info!(
                    "[simulate_linux_sysdata] Directory creation result for {:?}: {}",
                    proc_path,
                    e
                ),
            }

            log::info!(
                "[simulate_linux_sysdata] Creating directory: {:?}",
                sys_path
            );
            match fs::create_dir_all(&sys_path) {
                Ok(_) => log::info!(
                    "[simulate_linux_sysdata] Successfully created directory: {:?}",
                    sys_path
                ),
                Err(e) => log::info!(
                    "[simulate_linux_sysdata] Directory creation result for {:?}: {}",
                    sys_path,
                    e
                ),
            }

            log::info!(
                "[simulate_linux_sysdata] Creating directory: {:?}",
                sys_empty_path
            );
            match fs::create_dir_all(&sys_empty_path) {
                Ok(_) => log::info!(
                    "[simulate_linux_sysdata] Successfully created directory: {:?}",
                    sys_empty_path
                ),
                Err(e) => log::info!(
                    "[simulate_linux_sysdata] Directory creation result for {:?}: {}",
                    sys_empty_path,
                    e
                ),
            }

            // Set permissions using Android's built-in chmod
            log::info!(
                "[simulate_linux_sysdata] Setting permissions 700 on {:?}",
                proc_path
            );
            match std::process::Command::new("chmod")
                .arg("700")
                .arg(&proc_path)
                .output()
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    log::info!(
                        "[simulate_linux_sysdata] chmod 700 on proc - exit code: {:?}",
                        output.status.code()
                    );
                    log::info!("[simulate_linux_sysdata] chmod proc stdout: {}", stdout);
                    log::info!("[simulate_linux_sysdata] chmod proc stderr: {}", stderr);
                    if !output.status.success() {
                        log::error!(
                            "[simulate_linux_sysdata] chmod 700 on proc failed with status: {:?}",
                            output.status
                        );
                    }
                }
                Err(e) => log::error!(
                    "[simulate_linux_sysdata] Failed to execute chmod on proc: {}",
                    e
                ),
            }

            log::info!(
                "[simulate_linux_sysdata] Setting permissions 700 on {:?}",
                sys_path
            );
            match std::process::Command::new("chmod")
                .arg("700")
                .arg(&sys_path)
                .output()
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    log::info!(
                        "[simulate_linux_sysdata] chmod 700 on sys - exit code: {:?}",
                        output.status.code()
                    );
                    log::info!("[simulate_linux_sysdata] chmod sys stdout: {}", stdout);
                    log::info!("[simulate_linux_sysdata] chmod sys stderr: {}", stderr);
                    if !output.status.success() {
                        log::error!(
                            "[simulate_linux_sysdata] chmod 700 on sys failed with status: {:?}",
                            output.status
                        );
                    }
                }
                Err(e) => log::error!(
                    "[simulate_linux_sysdata] Failed to execute chmod on sys: {}",
                    e
                ),
            }

            log::info!(
                "[simulate_linux_sysdata] Setting permissions 700 on {:?}",
                sys_empty_path
            );
            match std::process::Command::new("chmod")
                .arg("700")
                .arg(&sys_empty_path)
                .output()
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    log::info!(
                        "[simulate_linux_sysdata] chmod 700 on sys/.empty - exit code: {:?}",
                        output.status.code()
                    );
                    log::info!(
                        "[simulate_linux_sysdata] chmod sys/.empty stdout: {}",
                        stdout
                    );
                    log::info!(
                        "[simulate_linux_sysdata] chmod sys/.empty stderr: {}",
                        stderr
                    );
                    if !output.status.success() {
                        log::error!("[simulate_linux_sysdata] chmod 700 on sys/.empty failed with status: {:?}", output.status);
                    }
                }
                Err(e) => log::error!(
                    "[simulate_linux_sysdata] Failed to execute chmod on sys/.empty: {}",
                    e
                ),
            }

            // Create fake proc files
            let proc_files = [
                    ("proc/.loadavg", "0.12 0.07 0.02 2/165 765\n"),
                    ("proc/.stat", "cpu  1957 0 2877 93280 262 342 254 87 0 0\ncpu0 31 0 226 12027 82 10 4 9 0 0\n"),
                    ("proc/.uptime", "124.08 932.80\n"),
                    ("proc/.version", "Linux version 6.2.1 (proot@termux) (gcc (GCC) 12.2.1 20230201, GNU ld (GNU Binutils) 2.40) #1 SMP PREEMPT_DYNAMIC Wed, 01 Mar 2023 00:00:00 +0000\n"),
                    ("proc/.vmstat", "nr_free_pages 1743136\nnr_zone_inactive_anon 179281\nnr_zone_active_anon 7183\n"),
                    ("proc/.sysctl_entry_cap_last_cap", "40\n"),
                    ("proc/.sysctl_inotify_max_user_watches", "4096\n"),
                ];

            log::info!(
                "[simulate_linux_sysdata] Creating {} proc files",
                proc_files.len()
            );
            for (path, content) in proc_files {
                let file_path = fs_root.join(path);
                log::info!("[simulate_linux_sysdata] Writing file: {:?}", file_path);
                match fs::write(&file_path, content) {
                    Ok(_) => log::info!(
                        "[simulate_linux_sysdata] Successfully wrote file: {:?}",
                        file_path
                    ),
                    Err(e) => {
                        log::error!(
                            "[simulate_linux_sysdata] Failed to write file {:?}: {}",
                            file_path,
                            e
                        );
                        panic!("Permission denied while writing to {}", path);
                    }
                }
            }

            log::info!("[simulate_linux_sysdata] System data simulation completed");
        }));
    }
    None
}

fn install_dependencies(options: &SetupOptions) -> StageOutput {
    let SetupOptions {
        mpsc_sender,
        android_app: _,
    } = options;

    let context = get_application_context();
    let CommandConfig {
        check,
        install,
        launch: _,
    } = context.local_config.command;

    let installed = move || {
        ArchProcess::exec(&check)
            .wait()
            .pb_expect("Failed to check whether the installation target is installed")
            .success()
    };

    if installed() {
        return None;
    }

    let mpsc_sender = mpsc_sender.clone();
    return Some(thread::spawn(move || {
        const MAX_INSTALL_ATTEMPTS: usize = 10;

        // Install dependencies until `check` succeeds.
        for attempt in 1..=MAX_INSTALL_ATTEMPTS {
            mpsc_sender
                .send(SetupMessage::Progress(format!(
                    "Installing desktop dependencies (attempt {}/{})...",
                    attempt, MAX_INSTALL_ATTEMPTS
                )))
                .pb_expect("Failed to send dependency install progress");

            ArchProcess::exec_with_panic_on_error("rm -f /var/lib/pacman/db.lck");
            let install_with_stderr = format!("({}) 2>&1", install);
            let mut saw_execve_enosys = false;
            let install_status = ArchProcess::exec(&install_with_stderr)
                .with_log(|it| {
                    if ArchProcess::is_execve_enosys(&it) {
                        saw_execve_enosys = true;
                    }
                    log::info!("Dependency install output: {}", it);
                    mpsc_sender
                        .send(SetupMessage::Progress(it))
                        .pb_expect("Failed to send log message");
                })
                .pb_expect("Failed while running desktop dependency install command");

            if !install_status.success() {
                if saw_execve_enosys && !ArchProcess::no_seccomp_enabled() {
                    ArchProcess::enable_no_seccomp_fallback("dependency install execve ENOSYS");
                    mpsc_sender
                        .send(SetupMessage::Progress(
                            "Detected device PRoot ENOSYS issue, enabling compatibility fallback..."
                                .to_string(),
                        ))
                        .unwrap_or(());
                }
                if saw_execve_enosys {
                    log::error!(
                        "PROOT_EXECVE_ENOSYS_DETECTED phase=install_dependencies attempt={} no_seccomp={}",
                        attempt,
                        ArchProcess::no_seccomp_enabled()
                    );
                }
                log::warn!(
                    "Dependency install command exited with status: {:?}, saw_execve_enosys={}, no_seccomp={}",
                    install_status.code(),
                    saw_execve_enosys,
                    ArchProcess::no_seccomp_enabled()
                );
            }

            if installed() {
                return;
            }

            if attempt == MAX_INSTALL_ATTEMPTS {
                let error_message = format!(
                    "Failed to install desktop dependencies after {} attempts. Check network/repo health and package availability.",
                    MAX_INSTALL_ATTEMPTS
                );
                emit_setup_error(&mpsc_sender, error_message.clone());
                panic!("{}", error_message);
            }
        }
    }));
}

fn configure_pacman_for_android(options: &SetupOptions) -> StageOutput {
    let mpsc_sender = options.mpsc_sender.clone();
    let fs_root = Path::new(ARCH_FS_ROOT);
    let pacman_conf_path = fs_root.join("etc/pacman.conf");

    if !pacman_conf_path.exists() {
        return None;
    }

    mpsc_sender
        .send(SetupMessage::Progress(
            "Configuring pacman for Android runtime...".to_string(),
        ))
        .unwrap_or(());

    let content =
        fs::read_to_string(&pacman_conf_path).pb_expect("Failed to read pacman configuration");
    let mut changed = false;
    let mut lines: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim_start();
        let indent_len = line.len() - trimmed.len();
        let indent = &line[..indent_len];

        if trimmed.starts_with("DownloadUser") {
            lines.push(format!("{}# {}", indent, trimmed));
            changed = true;
            continue;
        }

        if trimmed.starts_with("ParallelDownloads") {
            let desired = format!("{}ParallelDownloads = 1", indent);
            if line != desired {
                changed = true;
            }
            lines.push(desired);
            continue;
        }

        if trimmed.starts_with("SigLevel") {
            let desired = format!("{}SigLevel = Never", indent);
            if line != desired {
                changed = true;
            }
            lines.push(desired);
            continue;
        }

        if trimmed.starts_with("LocalFileSigLevel") {
            let desired = format!("{}LocalFileSigLevel = Never", indent);
            if line != desired {
                changed = true;
            }
            lines.push(desired);
            continue;
        }

        lines.push(line.to_string());
    }

    if changed {
        let mut updated = lines.join("\n");
        updated.push('\n');
        fs::write(&pacman_conf_path, updated)
            .pb_expect("Failed to update pacman configuration for Android");
    }

    let sync_dir = fs_root.join("var/lib/pacman/sync");
    let pkg_cache_dir = fs_root.join("var/cache/pacman/pkg");
    fs::create_dir_all(&sync_dir).pb_expect("Failed to create pacman sync directory");
    fs::create_dir_all(&pkg_cache_dir).pb_expect("Failed to create pacman package cache directory");

    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&sync_dir, fs::Permissions::from_mode(0o755));
        let _ = fs::set_permissions(&pkg_cache_dir, fs::Permissions::from_mode(0o755));
    }

    None
}

fn setup_firefox_config(_: &SetupOptions) -> StageOutput {
    // Create the Firefox root directory if it doesn't exist
    let firefox_root = format!("{}/usr/lib/firefox", ARCH_FS_ROOT);
    let _ = fs::create_dir_all(&firefox_root).pb_expect("Failed to create Firefox root directory");

    // Create the defaults/pref directory
    let pref_dir = format!("{}/defaults/pref", firefox_root);
    let _ = fs::create_dir_all(&pref_dir).pb_expect("Failed to create Firefox pref directory");

    // Create autoconfig.js in defaults/pref
    let autoconfig_js = r#"pref("general.config.filename", "localdesktop.cfg");
pref("general.config.obscure_value", 0);
"#;

    let _ = fs::write(format!("{}/autoconfig.js", pref_dir), autoconfig_js)
        .pb_expect("Failed to write Firefox autoconfig.js");

    // Create localdesktop.cfg in the Firefox root directory
    let firefox_cfg = r#"// Auto updated by Local Desktop on each startup, do not edit manually
defaultPref("media.cubeb.sandbox", false);
defaultPref("security.sandbox.content.level", 0);
"#; // It is required that the first line of this file is a comment, even if you have nothing to comment. Docs: https://support.mozilla.org/en-US/kb/customizing-firefox-using-autoconfig

    let _ = fs::write(format!("{}/localdesktop.cfg", firefox_root), firefox_cfg)
        .pb_expect("Failed to write Firefox configuration");

    None
}

#[derive(Debug)]
enum KvLine {
    Entry {
        key: String,
        value: String,
        prefix: String,
        delimiter: char,
    },
    Other(String),
}

fn parse_kv_lines(content: &str, delimiter: char) -> Vec<KvLine> {
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                return KvLine::Other(line.to_string());
            }
            if let Some((left, right)) = line.split_once(delimiter) {
                let key = left.trim().to_string();
                if key.is_empty() {
                    return KvLine::Other(line.to_string());
                }
                let prefix_len = line.len() - trimmed.len();
                let prefix = line[..prefix_len].to_string();
                let value = right.trim().to_string();
                KvLine::Entry {
                    key,
                    value,
                    prefix,
                    delimiter,
                }
            } else {
                KvLine::Other(line.to_string())
            }
        })
        .collect()
}

fn set_kv_value(lines: &mut Vec<KvLine>, key: &str, value: &str, delimiter: char) {
    let mut updated = false;
    for line in lines.iter_mut() {
        if let KvLine::Entry {
            key: entry_key,
            value: entry_value,
            ..
        } = line
        {
            if entry_key == key {
                *entry_value = value.to_string();
                updated = true;
            }
        }
    }
    if !updated {
        lines.push(KvLine::Entry {
            key: key.to_string(),
            value: value.to_string(),
            prefix: String::new(),
            delimiter,
        });
    }
}

fn render_kv_lines(lines: &[KvLine]) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        match line {
            KvLine::Entry {
                key,
                value,
                prefix,
                delimiter,
            } => out.push(format!("{}{}{} {}", prefix, key, delimiter, value)),
            KvLine::Other(raw) => out.push(raw.to_string()),
        }
    }
    let mut content = out.join("\n");
    content.push('\n');
    content
}

fn upsert_kv_file(path: &Path, delimiter: char, updates: &[(&str, String)]) {
    let content = fs::read_to_string(path).unwrap_or_default();
    let mut lines = parse_kv_lines(&content, delimiter);
    for (key, value) in updates {
        set_kv_value(&mut lines, key, value, delimiter);
    }
    let content = render_kv_lines(&lines);
    fs::write(path, content).pb_expect("Failed to write key/value file");
}

fn update_ini_section(content: &str, section: &str, updates: &[(&str, String)]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut seen_section = false;
    let mut seen_keys = vec![false; updates.len()];

    for raw_line in content.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_section {
                for (idx, (key, value)) in updates.iter().enumerate() {
                    if !seen_keys[idx] {
                        out.push(format!("{}={}", key, value));
                    }
                }
            }
            let name = trimmed[1..trimmed.len() - 1].trim();
            in_section = name.eq_ignore_ascii_case(section);
            if in_section {
                seen_section = true;
            }
            out.push(raw_line.to_string());
            continue;
        }

        if in_section
            && !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && !trimmed.starts_with(';')
            && raw_line.contains('=')
        {
            if let Some((left, _)) = raw_line.split_once('=') {
                let key = left.trim();
                let mut replaced = false;
                for (idx, (target_key, value)) in updates.iter().enumerate() {
                    if key.eq_ignore_ascii_case(target_key) {
                        let indent: String =
                            raw_line.chars().take_while(|c| c.is_whitespace()).collect();
                        out.push(format!("{}{}={}", indent, key, value));
                        seen_keys[idx] = true;
                        replaced = true;
                        break;
                    }
                }
                if replaced {
                    continue;
                }
            }
        }

        out.push(raw_line.to_string());
    }

    if in_section {
        for (idx, (key, value)) in updates.iter().enumerate() {
            if !seen_keys[idx] {
                out.push(format!("{}={}", key, value));
            }
        }
    } else if !seen_section {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(format!("[{}]", section));
        for (key, value) in updates {
            out.push(format!("{}={}", key, value));
        }
    }

    let mut content = out.join("\n");
    content.push('\n');
    content
}

fn extract_attr_value(line: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_tag_value(line: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = line.find(&open)? + open.len();
    let end = line.find(&close)?;
    if end < start {
        return None;
    }
    Some(line[start..end].trim().to_string())
}

fn update_openbox_rc(content: &str, scale: i32, font_name: &str) -> (String, Option<String>) {
    let active_size = 10 * scale;
    let menu_size = 11 * scale;
    let mut out: Vec<String> = Vec::new();
    let mut in_font = false;
    let mut in_theme = false;
    let mut font_place: Option<String> = None;
    let mut theme_name: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<theme>") {
            in_theme = true;
            out.push(line.to_string());
            continue;
        }
        if trimmed.starts_with("</theme>") {
            in_theme = false;
            out.push(line.to_string());
            continue;
        }

        if trimmed.starts_with("<font") {
            in_font = true;
            font_place = extract_attr_value(trimmed, "place");
            out.push(line.to_string());
            continue;
        }
        if trimmed.starts_with("</font>") {
            in_font = false;
            font_place = None;
            out.push(line.to_string());
            continue;
        }

        if in_theme && !in_font && theme_name.is_none() {
            if let Some(name) = extract_tag_value(trimmed, "name") {
                theme_name = Some(name);
            }
            out.push(line.to_string());
            continue;
        }

        if in_font {
            if extract_tag_value(trimmed, "name").is_some() {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                out.push(format!("{}<name>{}</name>", indent, font_name));
                continue;
            }
            if extract_tag_value(trimmed, "size").is_some() {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                let size = match font_place.as_deref() {
                    Some("ActiveWindow") | Some("InactiveWindow") => active_size,
                    Some("MenuHeader")
                    | Some("MenuItem")
                    | Some("ActiveOnScreenDisplay")
                    | Some("InactiveOnScreenDisplay") => menu_size,
                    _ => menu_size,
                };
                out.push(format!("{}<size>{}</size>", indent, size));
                continue;
            }
        }

        out.push(line.to_string());
    }

    let mut out = out.join("\n");
    out.push('\n');
    (out, theme_name)
}

fn update_openbox_theme(fs_root: &Path, theme_name: &str, scale: i32) {
    let user_theme = fs_root.join(format!("root/.themes/{}/openbox-3/themerc", theme_name));
    let system_theme = fs_root.join(format!("usr/share/themes/{}/openbox-3/themerc", theme_name));
    let source = if user_theme.exists() {
        user_theme.clone()
    } else if system_theme.exists() {
        system_theme
    } else {
        return;
    };

    let content = fs::read_to_string(&source).unwrap_or_default();
    if content.is_empty() {
        return;
    }

    let button_size = 18 * scale;
    let title_height = 22 * scale;
    let mut lines = parse_kv_lines(&content, ':');
    set_kv_value(&mut lines, "button.width", &button_size.to_string(), ':');
    set_kv_value(&mut lines, "button.height", &button_size.to_string(), ':');
    set_kv_value(&mut lines, "title.height", &title_height.to_string(), ':');

    let content = render_kv_lines(&lines);
    let _ = fs::create_dir_all(
        user_theme
            .parent()
            .pb_expect("Failed to read openbox theme directory"),
    );
    fs::write(&user_theme, content).pb_expect("Failed to write openbox theme file");
}

fn setup_lxqt_scaling(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let android_app = options.android_app.clone();

    let mut density_dpi: i32 = 160;
    run_in_jvm(
        |env, app| {
            let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as *mut _jobject) };
            let resources = env
                .call_method(
                    activity,
                    "getResources",
                    "()Landroid/content/res/Resources;",
                    &[],
                )
                .pb_expect("Failed to call getResources")
                .l()
                .pb_expect("Failed to read getResources result");
            let metrics = env
                .call_method(
                    resources,
                    "getDisplayMetrics",
                    "()Landroid/util/DisplayMetrics;",
                    &[],
                )
                .pb_expect("Failed to call getDisplayMetrics")
                .l()
                .pb_expect("Failed to read getDisplayMetrics result");
            density_dpi = env
                .get_field(metrics, "densityDpi", "I")
                .pb_expect("Failed to read densityDpi")
                .i()
                .pb_expect("Failed to convert densityDpi");
        },
        android_app,
    );

    let scale = ((density_dpi as f32) / 160.0 * 1.1).max(1.0).round() as i32;
    let xft_dpi = scale * 96;

    let xresources_path = fs_root.join("root/.Xresources");
    upsert_kv_file(&xresources_path, ':', &[("Xft.dpi", xft_dpi.to_string())]);

    let session_path = fs_root.join("root/.config/lxqt/session.conf");
    let _ = fs::create_dir_all(
        session_path
            .parent()
            .pb_expect("Failed to read LXQt session.conf parent directory"),
    );

    let session_content = fs::read_to_string(&session_path).unwrap_or_default();
    let session_with_env = update_ini_section(
        &session_content,
        "Environment",
        &[
            ("GDK_SCALE", scale.to_string()),
            ("QT_SCALE_FACTOR", scale.to_string()),
        ],
    );
    let session_out = update_ini_section(
        &session_with_env,
        "General",
        &[("window_manager", "openbox".to_string())],
    );
    fs::write(&session_path, session_out).pb_expect("Failed to write session.conf");

    // lxqt-powermanagement frequently crashes in a PRoot container due to missing
    // host power-management interfaces. Disable its autostart by default.
    let autostart_dir = fs_root.join("root/.config/autostart");
    let _ = fs::create_dir_all(&autostart_dir);
    let powermanagement_override = autostart_dir.join("lxqt-powermanagement.desktop");
    let powermanagement_hidden = r#"[Desktop Entry]
Type=Application
Name=LXQt Power Management
Hidden=true
"#;
    fs::write(&powermanagement_override, powermanagement_hidden)
        .pb_expect("Failed to disable lxqt-powermanagement autostart");

    let openbox_user_rc = fs_root.join("root/.config/openbox/rc.xml");
    let openbox_system_rc = fs_root.join("etc/xdg/openbox/rc.xml");
    let openbox_source = if openbox_user_rc.exists() {
        openbox_user_rc.clone()
    } else if openbox_system_rc.exists() {
        openbox_system_rc
    } else {
        return None;
    };

    let rc_content = fs::read_to_string(&openbox_source).unwrap_or_default();
    if !rc_content.is_empty() {
        let (rc_out, theme_name) = update_openbox_rc(&rc_content, scale, "DejaVu Sans");
        let _ = fs::create_dir_all(
            openbox_user_rc
                .parent()
                .pb_expect("Failed to read openbox config directory"),
        );
        fs::write(&openbox_user_rc, rc_out).pb_expect("Failed to write openbox rc.xml");

        if let Some(theme_name) = theme_name {
            update_openbox_theme(fs_root, &theme_name, scale);
        }
    }

    None
}

fn fix_xkb_symlink(_options: &SetupOptions) -> StageOutput {
    // Skip xkb symlink fix for Ubuntu rootfs
    None
}

/// Patch Ubuntu rootfs with fake proc files and Android-specific configurations
fn patch_ubuntu_rootfs(options: &SetupOptions) -> StageOutput {
    let fs_root = Path::new(ARCH_FS_ROOT);
    let mpsc_sender = options.mpsc_sender.clone();

    // Check if already patched
    if fs_root.join("etc/hosts.patched").exists() {
        return None;
    }

    mpsc_sender
        .send(SetupMessage::Progress(
            "Patching Ubuntu rootfs...".to_string(),
        ))
        .unwrap_or(());

    Some(thread::spawn(move || {
        log::info!("[patch_ubuntu] Starting rootfs patching");

        // Create necessary directories
        let _ = fs::create_dir_all(fs_root.join("dev"));
        let _ = fs::create_dir_all(fs_root.join("sys"));
        let _ = fs::create_dir_all(fs_root.join("proc"));
        let _ = fs::create_dir_all(fs_root.join("dev/shm"));

        // Set permissions on /proc
        let _ = std::process::Command::new("chmod")
            .arg("700")
            .arg(fs_root.join("proc"))
            .output();

        // Create fake /proc/version
        let version_content = "Linux version 5.19.0-76051900-faked (udroid@RandomCoder.org) #202207312230~1660780566~22.04~9d60db1 SMP PREEMPT_DYNAMIC Thu A\n";
        let _ = fs::write(fs_root.join("proc/.version"), version_content);

        // Create fake /proc/uptime
        let _ = fs::write(fs_root.join("proc/.uptime"), "7857.09 54258.46\n");

        // Create fake /proc/loadavg
        let _ = fs::write(
            fs_root.join("proc/.loadavg"),
            "16.98 17.85 18.62 1/4050 18463\n",
        );

        // Create fake /proc/stat (minimal)
        let _ = fs::write(fs_root.join("proc/.stat"), "cpu  240441 136982 262130 1546780 8977 0 12736 0 0 0\ncpu0 41348 30244 47145 148451 681 0 4488 0 0 0\nintr 15450380 0 0 0 0 0 0 0 675060 660856 664695 670871 510571 494303 405240 318695 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 55049 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 915 0 0 24484 0 3948 0 3948 0 0 14320 0 0 0 0 0 14320 0 0 0 188358 0 0 2 0 0 0 0 0 0 0 0 0 0 0 0 14 0 0 0 1091 0 1145821 0 0 0 2064 5105 0 2 2978 143260 36588 175214 2310 144623 667 722008 0 0 0 7060 0 0 19562 19561 19567 19564 20665 20665 20048 20045 24678 20666 20665 0 4248 0 0 74320 68 143602 21527 0 142 0 266 0 26 0 0 0 0 2078 70 0 0 0 1 152 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0 9160 0 0 1 51976 5 0 0 0 8028 0 0 0 1\nctxt 20705676\nbtime 1666933600\nprocesses 77409\nprocs_running 3\nprocs_blocked 0\nsoftirq 8877573 71 2578491 2414 766392 698255 0 14365 2439309 0 2378276\n");

        // Create fake /proc/vmstat (minimal)
        let _ = fs::write(fs_root.join("proc/.vmstat"), "nr_free_pages 797479\nnr_zone_inactive_anon 1350842\nnr_zone_active_anon 5792\nnr_zone_inactive_file 452524\nnr_zone_active_file 1235888\nnr_zone_unevictable 40\nnr_zone_write_pending 21\nnr_mlock 40\nnr_bounce 0\nnr_zspages 0\nnr_free_cma 0\nnuma_hit 62193717\nnuma_miss 0\nnuma_foreign 0\nnuma_interleave 1685\nnuma_local 62193717\nnuma_other 0\nnr_inactive_anon 1350842\nnr_active_anon 5792\nnr_inactive_file 452524\nnr_active_file 1235888\nnr_unevictable 40\nnr_slab_reclaimable 90461\nnr_slab_unreclaimable 46994\nnr_isolated_anon 0\nnr_isolated_file 0\nworkingset_nodes 26540\nworkingset_refault_anon 30\nworkingset_refault_file 61857\nworkingset_activate_anon 29\nworkingset_activate_file 58699\nworkingset_restore_anon 8\nworkingset_restore_file 10680\nworkingset_nodereclaim 1792\nnr_anon_pages 1258098\nnr_mapped 336800\nnr_file_pages 1787020\nnr_dirty 21\nnr_writeback 0\nnr_writeback_temp 0\nnr_shmem 100931\nnr_shmem_hugepages 0\nnr_shmem_pmdmapped 0\nnr_file_hugepages 0\nnr_file_pmdmapped 0\nnr_anon_transparent_hugepages 0\nnr_vmscan_write 199\nnr_vmscan_immediate_reclaim 64\nnr_dirtied 3125493\nnr_written 2724601\nnr_throttled_written 0\nnr_kernel_misc_reclaimable 0\nnr_foll_pin_acquired 0\nnr_foll_pin_released 0\nnr_kernel_stack 24176\nnr_page_table_pages 15826\nnr_swapcached 47\npgpromote_success 0\nnr_dirty_threshold 65536\nnr_dirty_background_threshold 32768\npgpgin 3980696\npgpgout 11524509\npswpin 30\npswpout 199\npgalloc_dma 1\npgalloc_dma32 3665609\npgalloc_normal 58548953\npgalloc_movable 0\nallocstall_dma 0\nallocstall_dma32 0\nallocstall_normal 83\nallocstall_movable 24\npgskip_dma 0\npgskip_dma32 0\npgskip_normal 0\npgskip_movable 0\npgfree 63437677\npgactivate 2588607\npgdeactivate 289583\npglazyfree 28031\npgfault 41043642\npgmajfault 17041\npglazyfreed 0\npgrefill 318961\npgreuse 4096458\npgsteal_kswapd 1325091\npgsteal_direct 21698\npgdemote_kswapd 0\npgdemote_direct 0\npgscan_kswapd 1589709\npgscan_direct 23668\npgscan_direct_throttle 0\npgscan_anon 55038\npgscan_file 1558339\npgsteal_anon 194\npgsteal_file 1346595\nzone_reclaim_failed 0\npginodesteal 0\nslabs_scanned 327296\nkswapd_inodesteal 1010\nkswapd_low_wmark_hit_quickly 276\nkswapd_high_wmark_hit_quickly 38\npageoutrun 474\npgrotated 436\ndrop_pagecache 0\ndrop_slab 0\noom_kill 0\nnuma_pte_updates 0\nnuma_huge_pte_updates 0\nnuma_hint_faults 0\nnuma_hint_faults_local 0\nnuma_pages_migrated 0\npgmigrate_success 345763\npgmigrate_fail 90\nthp_migration_success 0\nthp_migration_fail 0\nthp_migration_split 0\ncompact_migrate_scanned 2693820\ncompact_free_scanned 14772930\ncompact_isolated 704787\ncompact_stall 0\ncompact_fail 0\ncompact_success 0\ncompact_daemon_wake 290\ncompact_daemon_migrate_scanned 86861\ncompact_daemon_free_scanned 797667\nhtlb_buddy_alloc_success 0\nhtlb_buddy_alloc_fail 0\nunevictable_pgs_culled 369346\nunevictable_pgs_scanned 0\nunevictable_pgs_rescued 271919\nunevictable_pgs_mlocked 274444\nunevictable_pgs_munlocked 274400\nunevictable_pgs_cleared 0\nunevictable_pgs_stranded 4\nthp_fault_alloc 1\nthp_fault_fallback 0\nthp_fault_fallback_charge 0\nthp_collapse_alloc 0\nthp_collapse_alloc_failed 0\nthp_file_alloc 0\nthp_file_fallback 0\nthp_file_fallback_charge 0\nthp_file_mapped 0\nthp_split_page 0\nthp_split_page_failed 0\nthp_deferred_split_page 0\nthp_split_pmd 0\nthp_scan_exceed_none_pte 0\nthp_scan_exceed_swap_pte 0\nthp_scan_exceed_share_pte 0\nthp_split_pud 0\nthp_zero_page_alloc 0\nthp_zero_page_alloc_failed 0\nthp_swpout 0\nthp_swpout_fallback 0\nballoon_inflate 0\nballoon_deflate 0\nballoon_migrate 0\nswap_ra 21\nswap_ra_hit 7\nksm_swpin_copy 0\ncow_ksm 0\nzswpin 0\nzswpout 0\ndirect_map_level2_splits 409\ndirect_map_level3_splits 9\nnr_unstable 0\n");

        // Create /etc/hosts for connectivity
        let hosts_content = "127.0.0.1 localhost\n127.0.0.1 localhost.localdomain\n127.0.0.1 local\n255.255.255.255 broadcasthost\n::1 localhost\n::1 ip6-localhost\n::1 ip6-loopback\nfe80::1%lo0 localhost\nff00::0 ip6-localnet\nff00::0 ip6-mcastprefix\nff02::1 ip6-allnodes\nff02::2 ip6-allrouters\nff02::3 ip6-allhosts\n";
        let _ = fs::write(fs_root.join("etc/hosts"), hosts_content);

        // Create /etc/resolv.conf for DNS
        let resolv_content = "nameserver 1.1.1.1\nnameserver 8.8.8.8\n";
        let _ = fs::write(fs_root.join("etc/resolv.conf"), resolv_content);

        // Fix sudo permissions
        let sudo_path = fs_root.join("usr/bin/sudo");
        if sudo_path.exists() {
            let _ = std::process::Command::new("chmod")
                .arg("u+s")
                .arg(&sudo_path)
                .output();
        }

        // Create marker file to indicate patching is complete
        let _ = fs::write(fs_root.join("etc/hosts.patched"), "1\n");

        log::info!("[patch_ubuntu] Rootfs patching completed");
    }))
}

pub fn setup(android_app: AndroidApp) -> PolarBearBackend {
    let (sender, receiver) = mpsc::channel();
    let progress = Arc::new(Mutex::new(0));

    if ArchProcess::is_supported() {
        sender
            .send(SetupMessage::Progress(
                "✅ Your device is supported!".to_string(),
            ))
            .unwrap_or(());
    } else {
        log::info!("PRoot support check failed, showing Device Unsupported page");
        return PolarBearBackend::WebView(WebviewBackend {
            socket_port: 0,
            progress,
            error: ErrorVariant::Unsupported,
        });
    }

    let options = SetupOptions {
        android_app,
        mpsc_sender: sender.clone(),
    };

    let stages: Vec<SetupStage> = vec![
        Box::new(setup_arch_fs),                // Step 1. Setup Ubuntu FS (extract)
        Box::new(simulate_linux_sysdata_stage), // Step 2. Simulate Linux system data
        Box::new(patch_ubuntu_rootfs),          // Step 3. Patch Ubuntu rootfs
        Box::new(setup_firefox_config),         // Step 4. Setup Firefox config
    ];

    let handle_stage_error = |e: Box<dyn std::any::Any + Send>, sender: &Sender<SetupMessage>| {
        let error_msg = if let Some(e) = e.downcast_ref::<String>() {
            format!("Stage execution failed: {}", e)
        } else if let Some(e) = e.downcast_ref::<&str>() {
            format!("Stage execution failed: {}", e)
        } else {
            "Stage execution failed: Unknown error".to_string()
        };
        emit_setup_error(sender, error_msg);
    };

    let fully_installed = 'outer: loop {
        for (i, stage) in stages.iter().enumerate() {
            if let Some(handle) = stage(&options) {
                let progress_clone = progress.clone();
                let sender_clone = sender.clone();
                thread::spawn(move || {
                    let progress = progress_clone;
                    let progress_value = ((i) as u16 * 100 / stages.len() as u16) as u16;
                    *progress.lock().unwrap() = progress_value;

                    // Wait for the current stage to finish
                    if let Err(e) = handle.join() {
                        handle_stage_error(e, &sender_clone);
                        return;
                    }

                    // Process the remaining stages in the same loop
                    for (j, next_stage) in stages.iter().enumerate().skip(i + 1) {
                        let progress_value = ((j) as u16 * 100 / stages.len() as u16) as u16;
                        *progress.lock().unwrap() = progress_value;
                        if let Some(next_handle) = next_stage(&options) {
                            if let Err(e) = next_handle.join() {
                                handle_stage_error(e, &sender_clone);
                                return;
                            }

                            // Increment progress and send it
                            let next_progress_value =
                                ((j + 1) as u16 * 100 / stages.len() as u16) as u16;
                            *progress.lock().unwrap() = next_progress_value;
                        }
                    }

                    // All stages are done, we need to replace the WebviewBackend with the WaylandBackend
                    // Or, easier, just restart the whole app
                    *progress.lock().unwrap() = 100;
                    sender_clone
                        .send(SetupMessage::Progress(
                            "Installation finished, please restart the app".to_string(),
                        ))
                        .pb_expect("Failed to send installation finished message");
                });

                // Setup is still running in the background, but we need to return control
                // so that the main thread can continue to report progress to the user
                break 'outer false;
            }
        }

        // All stages were done previously, no need to wait for anything
        break 'outer true;
    };

    if fully_installed {
        PolarBearBackend::Wayland(WaylandBackend {
            compositor: Compositor::build().pb_expect("Failed to build compositor"),
            graphic_renderer: None,
            clock: Clock::new(),
            key_counter: 0,
            scale_factor: 1.0,
        })
    } else {
        PolarBearBackend::WebView(WebviewBackend::build(receiver, progress))
    }
}
