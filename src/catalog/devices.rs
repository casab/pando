//! Where an app a device runs is opened from this machine: the iOS
//! simulator and an Android device or emulator, how pando finds one that
//! is ready, and how it starts the simulator when none is.
//!
//! The commands that open the app on each are the framework's, in its
//! [`super::frameworks::Device`] row: which one a target runs is its
//! [`Platform`].

/// Which of a framework's open commands a target runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// [`super::frameworks::Device::simulator`].
    Ios,
    /// [`super::frameworks::Device::android`].
    Android,
}

/// Somewhere a device app is opened, in the order pando tries them.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub platform: Platform,
    /// What it is, as a sentence names it.
    pub name: &'static str,
    /// The shell command that lists the ones there are.
    pub list: &'static str,
    /// The last word of a line of that list that names one ready to open
    /// an app: the header and a device still booting end in another.
    pub ready: &'static str,
    /// Words in a failed open's output that say the target is not done
    /// starting and is worth another try: `simctl openurl` right after a
    /// cold boot times out with `code=60`.
    pub starting: &'static [&'static str],
    /// Words in an open's output that say it failed, though it exited 0:
    /// `am start` reports an intent nothing handles on its output alone.
    pub refused: &'static [&'static str],
}

/// The simulator first: it shares this machine's `127.0.0.1`, and it is
/// what Expo's own "press i" opened.
pub const TARGETS: [Target; 2] = [
    Target {
        platform: Platform::Ios,
        name: "the booted iOS simulator",
        list: "xcrun simctl list devices booted",
        ready: "(Booted)",
        starting: &["code=60"],
        refused: &[],
    },
    // `emulator-5554	device`; `unauthorized` and `offline` are not ready.
    Target {
        platform: Platform::Android,
        name: "the connected Android device or emulator",
        list: "adb devices",
        ready: "device",
        starting: &[],
        // `Error: Activity not started, unable to resolve Intent`: no app
        // on the device registers the link's scheme.
        refused: &["Error: "],
    },
];

/// How the iOS simulator is started when none is booted: its app, which
/// ships inside Xcode, and which boots the last device it ran.
#[derive(Debug, Clone, Copy)]
pub struct SimulatorApp {
    /// The shell command that prints the developer directory of the
    /// Xcode in use.
    pub developer_dir: &'static str,
    /// Where Xcode keeps its apps, relative to that directory.
    pub apps_dir: &'static str,
    /// The simulator app's name, whichever the Xcode in use ships:
    /// `Simulator.app` up to Xcode 26, `DeviceHub.app` from Xcode 27.
    pub names: &'static [&'static str],
    /// The shell command that starts the app at `{app}`, which pando
    /// fills in quoted.
    pub launch: &'static str,
}

pub const SIMULATOR_APP: SimulatorApp = SimulatorApp {
    developer_dir: "xcode-select -p",
    apps_dir: "../Applications",
    names: &["Simulator.app", "DeviceHub.app"],
    launch: "open -a {app}",
};

impl Target {
    /// Whether a line of [`Target::list`]'s output names one ready.
    pub fn lists_one_ready(&self, output: &str) -> bool {
        output
            .lines()
            .any(|line| line.split_whitespace().next_back() == Some(self.ready))
    }

    /// Whether a failed open's output says the target is still starting.
    pub fn still_starting(&self, output: &str) -> bool {
        self.starting.iter().any(|word| output.contains(word))
    }

    /// Whether an open that exited 0 says it failed all the same.
    pub fn refuses(&self, output: &str) -> bool {
        self.refused.iter().any(|word| output.contains(word))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(platform: Platform) -> &'static Target {
        TARGETS.iter().find(|t| t.platform == platform).unwrap()
    }

    // As `xcrun simctl list devices booted` prints them: one booted, the
    // header lines around it not.
    #[test]
    fn a_booted_simulator_is_a_line_ending_in_booted() {
        let ios = target(Platform::Ios);
        let none = "== Devices ==\n-- iOS 27.0 --\n";
        assert!(!ios.lists_one_ready(none));
        let one =
            format!("{none}    iPhone 17 Pro (5E1B7C0A-0000-4000-8000-000000000000) (Booted) \n");
        assert!(ios.lists_one_ready(&one));
        assert!(ios.still_starting(
            "An error was encountered processing the command (domain=NSPOSIXErrorDomain, code=60)"
        ));
        assert!(!ios.still_starting("No application is registered for the URL"));
    }

    // `adb devices`: a device ready ends in `device`; the header, one
    // not yet allowed on the phone, and one offline do not.
    #[test]
    fn a_ready_android_device_is_a_line_ending_in_device() {
        let android = target(Platform::Android);
        let header = "List of devices attached\n";
        assert!(!android.lists_one_ready(header));
        assert!(!android.lists_one_ready(&format!("{header}R58M\tunauthorized\n")));
        assert!(!android.lists_one_ready(&format!("{header}emulator-5554\toffline\n")));
        assert!(android.lists_one_ready(&format!("{header}emulator-5554\tdevice\n")));
        // `am start` says so on its output when nothing handles the link.
        assert!(android.refuses(
            "Starting: Intent { act=android.intent.action.VIEW }\nError: Activity not \
             started, unable to resolve Intent { act=android.intent.action.VIEW }"
        ));
        assert!(!android.refuses("Starting: Intent { act=android.intent.action.VIEW }"));
        assert!(!target(Platform::Ios).refuses(""));
    }

    // One target per platform, so each of a framework's commands is run
    // on exactly one.
    #[test]
    fn every_platform_has_one_target() {
        for platform in [Platform::Ios, Platform::Android] {
            assert_eq!(
                TARGETS.iter().filter(|t| t.platform == platform).count(),
                1,
                "{platform:?}"
            );
        }
        assert!(SIMULATOR_APP.launch.contains("{app}"));
    }
}
