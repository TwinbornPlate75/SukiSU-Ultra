use crate::defs::{KSU_MOUNT_SOURCE, NO_MOUNT_PATH, NO_TMPFS_PATH};
use crate::kpm;
use crate::module::{handle_updated_modules, prune_modules};
use crate::{assets, defs, ksucalls, restorecon, utils};
use anyhow::{Context, Result};
use log::{info, warn};
use rustix::fs::{MountFlags, mount};

use std::{
    fs,
    io::Write,
    os::unix::{
        fs::{symlink, PermissionsExt},
        process::CommandExt,
    },
    path::Path,
    process::{Command, Stdio},
};
pub fn on_post_data_fs() -> Result<()> {
    ksucalls::report_post_fs_data();

    utils::umask(0);

    #[cfg(unix)]
    let _ = catch_bootlog("logcat", vec!["logcat"]);
    #[cfg(unix)]
    let _ = catch_bootlog("dmesg", vec!["dmesg", "-w"]);

    if utils::has_magisk() {
        warn!("Magisk detected, skip post-fs-data!");
        return Ok(());
    }

    let safe_mode = utils::is_safe_mode();

    if safe_mode {
        warn!("safe mode, skip common post-fs-data.d scripts");
    } else {
        // Then exec common post-fs-data scripts
        if let Err(e) = crate::module::exec_common_scripts("post-fs-data.d", true) {
            warn!("exec common post-fs-data scripts failed: {e}");
        }
    }

    assets::ensure_binaries(true).with_context(|| "Failed to extract bin assets")?;

    // Start UID scanner daemon with highest priority
    start_uid_scanner_daemon()?;

    // tell kernel that we've mount the module, so that it can do some optimization
    ksucalls::report_module_mounted();

    // if we are in safe mode, we should disable all modules
    if safe_mode {
        warn!("safe mode, skip post-fs-data scripts and disable all modules!");
        if let Err(e) = crate::module::disable_all_modules() {
            warn!("disable all modules failed: {e}");
        }
        return Ok(());
    }

    if let Err(e) = prune_modules() {
        warn!("prune modules failed: {}", e);
    }

    if let Err(e) = handle_updated_modules() {
        warn!("handle updated modules failed: {}", e);
    }

    if let Err(e) = restorecon::restorecon() {
        warn!("restorecon failed: {e}");
    }

    // load sepolicy.rule
    if crate::module::load_sepolicy_rule().is_err() {
        warn!("load sepolicy.rule failed");
    }

    if let Err(e) = crate::profile::apply_sepolies() {
        warn!("apply root profile sepolicy failed: {e}");
    }

    // mount temp dir
    if !Path::new(NO_TMPFS_PATH).exists() {
        if let Err(e) = mount(
            KSU_MOUNT_SOURCE,
            utils::get_tmp_path(),
            "tmpfs",
            MountFlags::empty(),
            "",
        ) {
            warn!("do temp dir mount failed: {}", e);
        }
    } else {
        info!("no tmpfs requested");
    }

    // exec modules post-fs-data scripts
    // TODO: Add timeout
    if let Err(e) = crate::module::exec_stage_script("post-fs-data", true) {
        warn!("exec post-fs-data scripts failed: {e}");
    }

    // load system.prop
    if let Err(e) = crate::module::load_system_prop() {
        warn!("load system.prop failed: {e}");
    }

    // mount module systemlessly by magic mount
    if !Path::new(NO_MOUNT_PATH).exists() {
        if let Err(e) = mount_modules_systemlessly() {
            warn!("do systemless mount failed: {}", e);
        }
    } else {
        info!("no mount requested");
    }

    if kpm::is_kpm_enabled()? {
        kpm::start_kpm_watcher()?;
        kpm::load_kpm_modules()?;
    }

    run_stage("post-mount", true);

    Ok(())
}

#[cfg(target_os = "android")]
pub fn mount_modules_systemlessly() -> Result<()> {
    crate::magic_mount::magic_mount()
}

#[cfg(not(target_os = "android"))]
pub fn mount_modules_systemlessly() -> Result<()> {
    Ok(())
}

fn run_stage(stage: &str, block: bool) {
    utils::umask(0);

    if utils::has_magisk() {
        warn!("Magisk detected, skip {stage}");
        return;
    }

    if crate::utils::is_safe_mode() {
        warn!("safe mode, skip {stage} scripts");
        return;
    }

    if let Err(e) = crate::module::exec_common_scripts(&format!("{stage}.d"), block) {
        warn!("Failed to exec common {stage} scripts: {e}");
    }
    if let Err(e) = crate::module::exec_stage_script(stage, block) {
        warn!("Failed to exec {stage} scripts: {e}");
    }
}

pub fn on_services() -> Result<()> {
    info!("on_services triggered!");
    run_stage("service", false);

    Ok(())
}

pub fn on_boot_completed() -> Result<()> {
    ksucalls::report_boot_complete();
    info!("on_boot_completed triggered!");

    run_stage("boot-completed", false);

    Ok(())
}

fn start_uid_scanner_daemon() -> Result<()> {
    const SCANNER_PATH: &str = "/data/adb/uid_scanner";
    const LINK_DIR:  &str = "/data/adb/ksu/bin";
    const LINK_PATH: &str = "/data/adb/ksu/bin/uid_scanner";
    const SERVICE_DIR: &str = "/data/adb/service.d";
    const SERVICE_PATH: &str = "/data/adb/service.d/uid_scanner.sh";

    if !Path::new(SCANNER_PATH).exists() {
        warn!("uid scanner binary not found at {}", SCANNER_PATH);
        return Ok(());
    }

    if let Err(e) = fs::set_permissions(SCANNER_PATH, fs::Permissions::from_mode(0o755)) {
        warn!("failed to set permissions for {}: {}", SCANNER_PATH, e);
    }

    #[cfg(unix)]
    {
        if let Err(e) = fs::create_dir_all(LINK_DIR) {
            warn!("failed to create {}: {}", LINK_DIR, e);
        } else if !Path::new(LINK_PATH).exists() {
            match symlink(SCANNER_PATH, LINK_PATH) {
                Ok(_) => info!("created symlink {} -> {}", SCANNER_PATH, LINK_PATH),
                Err(e) => warn!("failed to create symlink: {}", e),
            }
        }
    }

    if let Err(e) = fs::create_dir_all(SERVICE_DIR) {
        warn!("failed to create {}: {}", SERVICE_DIR, e);
    } else if !Path::new(SERVICE_PATH).exists() {
        let content = r#"#!/system/bin/sh
# KSU uid_scanner auto-restart script
until [ -d "/sdcard/Android" ]; do sleep 1; done
sleep 10
/data/adb/uid_scanner restart
"#;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(SERVICE_PATH)
            .and_then(|mut f| {
                f.write_all(content.as_bytes())?;
                f.sync_all()?;
                // 0755
                fs::set_permissions(SERVICE_PATH, fs::Permissions::from_mode(0o755))
            }) {
            Ok(_) => info!("created service script {}", SERVICE_PATH),
            Err(e) => warn!("failed to write {}: {}", SERVICE_PATH, e),
        }
    }

    info!("starting uid scanner daemon with highest priority");
    let mut cmd = Command::new(SCANNER_PATH);
    cmd.arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/");

    unsafe {
        cmd.pre_exec(|| {
            libc::nice(-20);
            libc::setsid();
            Ok(())
        });
    }

    match cmd.spawn() {
        Ok(child) => {
            info!("uid scanner daemon started with pid: {}", child.id());
            std::mem::drop(child);
        }
        Err(e) => warn!("failed to start uid scanner daemon: {}", e),
    }

    Ok(())
}

#[cfg(unix)]
fn catch_bootlog(logname: &str, command: Vec<&str>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let logdir = Path::new(defs::LOG_DIR);
    utils::ensure_dir_exists(logdir)?;
    let bootlog = logdir.join(format!("{logname}.log"));
    let oldbootlog = logdir.join(format!("{logname}.old.log"));

    if bootlog.exists() {
        std::fs::rename(&bootlog, oldbootlog)?;
    }

    let bootlog = std::fs::File::create(bootlog)?;

    let mut args = vec!["-s", "9", "30s"];
    args.extend_from_slice(&command);
    
    let mut cmd = std::process::Command::new("timeout");
    cmd.process_group(0)
        .args(args)
        .stdout(Stdio::from(bootlog));

    unsafe {
        cmd.pre_exec(|| {
            utils::switch_cgroups();
            Ok(())
        });
    }

    // timeout -s 9 30s logcat > boot.log
    let result = cmd.spawn();

    if let Err(e) = result {
        warn!("Failed to start logcat: {e:#}");
    }

    Ok(())
}
