use super::process::ArchProcess;
use crate::android::utils::application_context::get_application_context;
use std::thread;

pub fn launch() {
    log::info!("[launch] Starting desktop launch sequence");
    thread::spawn(move || {
        log::info!("[launch] Spawned launch thread");

        // Clean up potential leftover files for display :1
        log::info!("[launch] Cleaning up X11 lock files");
        ArchProcess::exec("rm -f /tmp/.X1-lock");
        ArchProcess::exec("rm -f /tmp/.X11-unix/X1");

        let local_config = get_application_context().local_config;
        let username = local_config.user.username;

        log::info!("[launch] Launching desktop as user: {}", username);

        let full_launch_command = local_config.command.launch;
        log::info!("[launch] Full launch command: {}", full_launch_command);

        let run_launch = |saw_execve_enosys: &mut bool| {
            log::info!("[launch] Executing launch command in proot");
            ArchProcess::exec_as(&full_launch_command, &username).with_log(|it| {
                if ArchProcess::is_execve_enosys(&it) {
                    log::warn!("[launch] Detected execve ENOSYS error: {}", it);
                    *saw_execve_enosys = true;
                }
                log::trace!("[launch] Output: {}", it);
            })
        };

        let mut saw_execve_enosys = false;
        log::info!("[launch] Starting initial launch attempt");
        match run_launch(&mut saw_execve_enosys) {
            Ok(status) if !status.success() => {
                log::warn!(
                    "[launch] Desktop launch command exited with status: {:?}",
                    status.code()
                );
                if saw_execve_enosys && !ArchProcess::no_seccomp_enabled() {
                    log::warn!("[launch] ENOSYS detected, enabling no_seccomp fallback");
                    ArchProcess::enable_no_seccomp_fallback("desktop launch execve ENOSYS");
                    log::warn!(
                        "[launch] Retrying desktop launch with PROOT_NO_SECCOMP=1 after ENOSYS failure"
                    );
                    let mut saw_retry_execve_enosys = false;
                    log::info!("[launch] Starting retry launch attempt");
                    match run_launch(&mut saw_retry_execve_enosys) {
                        Ok(retry_status) if !retry_status.success() => {
                            log::error!(
                                "[launch] Desktop launch retry exited with status: {:?}",
                                retry_status.code()
                            );
                        }
                        Ok(_) => {
                            log::info!("[launch] Desktop launch retry succeeded");
                        }
                        Err(retry_err) => {
                            log::error!(
                                "[launch] Failed to run desktop launch retry command: {}",
                                retry_err
                            );
                        }
                    }
                }
            }
            Ok(_) => {
                log::info!("[launch] Desktop launch command completed successfully");
            }
            Err(err) => {
                log::warn!("[launch] Failed to run desktop launch command: {}", err);
            }
        }

        log::info!("[launch] Launch thread completed");
    });
}
