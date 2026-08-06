//! Deterministic delivery of rendered QR images to the operator channel.
//!
//! The model composes chat text, but attachment delivery must not depend on it:
//! an omitted marker silently means the operator receives no QR code. This
//! module invokes the trusted local host command directly instead.
//!
//! It carries no payment authority. It sends one bounded message containing a
//! validated receivable identifier and a marker derived from a local path.

use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::config::QrDeliveryConfig;

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Sends one QR attachment message and returns whether the host accepted it.
///
/// Delivery is fail-open by design: every failure is reported to stderr and
/// ignored, because a channel problem must never invalidate a created
/// receivable or a rendered QR artifact.
pub fn deliver_qr(config: &QrDeliveryConfig, receivable_id: &str, attachment_marker: &str) -> bool {
    if !attachment_marker.starts_with("[IMAGE:") || !attachment_marker.ends_with(']') {
        eprintln!("recebi-mcp QR delivery skipped: marker is not an image marker");
        return false;
    }
    let body = format!("🧾 QR code for {receivable_id}\n{attachment_marker}");
    // No shell is involved: arguments are passed directly to the host binary.
    let child = Command::new(&config.zeroclaw_bin)
        .arg("channel")
        .arg("send")
        .arg(&body)
        .arg("--channel-id")
        .arg(&config.channel_id)
        .arg("--recipient")
        .arg(&config.recipient)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        eprintln!("recebi-mcp QR delivery skipped: host command could not start");
        return false;
    };
    let deadline = Instant::now() + DELIVERY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return true;
                }
                eprintln!("recebi-mcp QR delivery failed: host command reported an error");
                return false;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("recebi-mcp QR delivery timed out");
                    return false;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                eprintln!("recebi-mcp QR delivery failed: host command could not be observed");
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use super::{QrDeliveryConfig, deliver_qr};

    fn stub(directory: &std::path::Path, body: &str) -> PathBuf {
        let path = directory.join("stub-host");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("stub");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("mode");
        path
    }

    fn config(binary: PathBuf) -> QrDeliveryConfig {
        QrDeliveryConfig {
            zeroclaw_bin: binary,
            channel_id: "telegram".to_owned(),
            recipient: "8428792550".to_owned(),
        }
    }

    #[test]
    fn sends_exact_bounded_arguments_to_the_trusted_host_command() {
        let directory = tempfile::tempdir().expect("dir");
        let record = directory.path().join("args.txt");
        let binary = stub(
            directory.path(),
            &format!("printf '%s\\n' \"$@\" > {}", record.display()),
        );
        assert!(deliver_qr(&config(binary), "INV-1", "[IMAGE:/tmp/qr.png]"));
        let arguments = fs::read_to_string(&record).expect("args");
        let arguments: Vec<&str> = arguments.lines().collect();
        assert_eq!(
            arguments,
            vec![
                "channel",
                "send",
                "🧾 QR code for INV-1",
                "[IMAGE:/tmp/qr.png]",
                "--channel-id",
                "telegram",
                "--recipient",
                "8428792550",
            ]
        );
    }

    #[test]
    fn rejects_a_non_image_marker_and_reports_host_failure() {
        let directory = tempfile::tempdir().expect("dir");
        let binary = stub(directory.path(), "exit 0");
        assert!(!deliver_qr(
            &config(binary.clone()),
            "INV-1",
            "[DOCUMENT:/tmp/report.csv]"
        ));
        let failing = stub(directory.path(), "exit 3");
        assert!(!deliver_qr(
            &config(failing),
            "INV-1",
            "[IMAGE:/tmp/qr.png]"
        ));
    }

    #[test]
    fn a_missing_host_command_is_fail_open() {
        let directory = tempfile::tempdir().expect("dir");
        assert!(!deliver_qr(
            &config(directory.path().join("absent")),
            "INV-1",
            "[IMAGE:/tmp/qr.png]"
        ));
    }
}
