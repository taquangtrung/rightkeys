//! Cross-platform best-effort desktop notifications. Failures (no `notify-send`,
//! no PowerShell, notifications disabled) are silently ignored.

// Constants

const APP_NAME: &str = "RightKeys";

#[cfg(target_os = "linux")]
const APP_ICON: &str = "rightkeys";

/// Auto-expire notifications after this many milliseconds. Critical urgency is
/// avoided so the daemon honors the timeout (critical notifications are resident
/// and never auto-dismiss per the freedesktop spec).
#[cfg(target_os = "linux")]
const EXPIRE_MS: &str = "5000";

// Functions

/// Show an informational notification (e.g. started, reloaded).
pub fn info(message: &str) {
    show(message, false);
}

/// Show a warning notification (e.g. an invalid config was rejected).
pub fn warn(message: &str) {
    show(message, true);
}

#[cfg(target_os = "linux")]
fn show(message: &str, warning: bool) {
    let icon = if warning { "dialog-warning" } else { APP_ICON };
    let _ = std::process::Command::new("notify-send")
        .args(["-a", APP_NAME, "-i", icon, "-t", EXPIRE_MS, message])
        .status();
}

#[cfg(windows)]
fn show(message: &str, _warning: bool) {
    let sanitized = message.replace('\'', "");
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null; \
         $t=[Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText01); \
         $t.GetElementsByTagName('text')[0].AppendChild($t.CreateTextNode('{sanitized}')) | Out-Null; \
         $n=[Windows.UI.Notifications.ToastNotification]::new($t); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{APP_NAME}').Show($n);"
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .spawn();
}

#[cfg(not(any(target_os = "linux", windows)))]
fn show(_message: &str, _warning: bool) {}
