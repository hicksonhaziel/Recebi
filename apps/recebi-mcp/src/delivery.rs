//! Deterministic delivery of rendered QR images to the operator channel.
//!
//! The model composes chat text, but attachment delivery must not depend on it:
//! an omitted marker silently means the operator receives no QR code. This
//! module invokes the trusted local host command directly instead.
//!
//! Delivery is deliberately delayed and non-blocking. A tool call returns in
//! milliseconds while the model still needs seconds to compose its reply, so an
//! immediate send would place the image before the message that explains it.
//! The delay is best-effort ordering, not a guarantee.
//!
//! This module confers no payment authority. It sends one bounded message
//! containing only a marker derived from a local path.

use std::{
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::config::QrDeliveryConfig;

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

fn pending() -> &'static Mutex<Vec<JoinHandle<()>>> {
    static PENDING: OnceLock<Mutex<Vec<JoinHandle<()>>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

/// Schedules one delayed QR delivery and returns immediately.
///
/// Returns `false` when the marker is not an image marker, so the caller can
/// fall back to reporting the marker to the operator.
pub fn schedule_qr_delivery(config: &QrDeliveryConfig, attachment_marker: &str) -> bool {
    if !attachment_marker.starts_with("[IMAGE:") || !attachment_marker.ends_with(']') {
        eprintln!("recebi-mcp QR delivery skipped: marker is not an image marker");
        return false;
    }
    let config = config.clone();
    let marker = attachment_marker.to_owned();
    let handle = thread::spawn(move || {
        thread::sleep(config.delay());
        send_now(&config, &marker);
    });
    if let Ok(mut handles) = pending().lock() {
        handles.retain(|handle| !handle.is_finished());
        handles.push(handle);
    }
    true
}

/// Waits for scheduled deliveries so a short-lived invocation still delivers.
///
/// The stdio server exits as soon as its input closes, which would otherwise
/// discard a pending delayed send.
pub fn wait_for_pending_deliveries() {
    let handles = pending()
        .lock()
        .map(|mut handles| std::mem::take(&mut *handles))
        .unwrap_or_default();
    for handle in handles {
        let _ = handle.join();
    }
}

/// Sends the marker as the entire message body.
///
/// The body carries no caption: `ZeroClaw` consumes the marker and uploads the
/// image, and any surrounding text would appear as a separate chat message
/// duplicating what the agent already said.
fn send_now(config: &QrDeliveryConfig, attachment_marker: &str) {
    // No shell is involved: arguments are passed directly to the host binary.
    let child = Command::new(&config.zeroclaw_bin)
        .arg("channel")
        .arg("send")
        .arg(attachment_marker)
        .arg("--channel-id")
        .arg(&config.channel_id)
        .arg("--recipient")
        .arg(&config.recipient)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        eprintln!("recebi-mcp QR delivery failed: host command could not start");
        return;
    };
    let deadline = Instant::now() + DELIVERY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    eprintln!("recebi-mcp QR delivery failed: host command reported an error");
                }
                return;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("recebi-mcp QR delivery timed out");
                    return;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                eprintln!("recebi-mcp QR delivery failed: host command could not be observed");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use super::{QrDeliveryConfig, schedule_qr_delivery, wait_for_pending_deliveries};

    fn stub(directory: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("stub");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("mode");
        path
    }

    fn config(binary: PathBuf) -> QrDeliveryConfig {
        QrDeliveryConfig {
            zeroclaw_bin: binary,
            channel_id: "telegram".to_owned(),
            recipient: "8428792550".to_owned(),
            delay_ms: Some(0),
        }
    }

    #[test]
    fn sends_only_the_marker_with_bounded_arguments() {
        let directory = tempfile::tempdir().expect("dir");
        let record = directory.path().join("args.txt");
        let binary = stub(
            directory.path(),
            "stub-host",
            &format!("printf '%s\\n' \"$@\" > {}", record.display()),
        );
        assert!(schedule_qr_delivery(&config(binary), "[IMAGE:/tmp/qr.png]"));
        wait_for_pending_deliveries();
        let arguments = fs::read_to_string(&record).expect("args");
        let arguments: Vec<&str> = arguments.lines().collect();
        assert_eq!(
            arguments,
            vec![
                "channel",
                "send",
                "[IMAGE:/tmp/qr.png]",
                "--channel-id",
                "telegram",
                "--recipient",
                "8428792550",
            ]
        );
    }

    #[test]
    fn rejects_a_marker_that_is_not_an_image() {
        let directory = tempfile::tempdir().expect("dir");
        let record = directory.path().join("never.txt");
        let binary = stub(
            directory.path(),
            "stub-reject",
            &format!("touch {}", record.display()),
        );
        assert!(!schedule_qr_delivery(
            &config(binary),
            "[DOCUMENT:/tmp/report.csv]"
        ));
        wait_for_pending_deliveries();
        assert!(!record.exists());
    }

    #[test]
    fn a_missing_or_failing_host_command_never_panics() {
        let directory = tempfile::tempdir().expect("dir");
        assert!(schedule_qr_delivery(
            &config(directory.path().join("absent")),
            "[IMAGE:/tmp/qr.png]"
        ));
        let failing = stub(directory.path(), "stub-fail", "exit 3");
        assert!(schedule_qr_delivery(
            &config(failing),
            "[IMAGE:/tmp/qr.png]"
        ));
        wait_for_pending_deliveries();
    }
}
