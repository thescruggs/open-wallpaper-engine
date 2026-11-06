// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// OpenRGB L4: the "LED sync" section of the Settings page. Everything comes
// from the `ledSync` client (led.status / led.reprobe / settings.set {led});
// the daemon is the source of truth. OpenRGB is optional: with no SDK server
// running this section is calm information, never an error.
//
// The enable switch covers "off"; the mode picker therefore lists only the
// three lighting modes. A daemon whose stored mode is "off" (set outside the
// manager) shows "Off" in the picker until another mode is chosen.
ColumnLayout {
    id: section

    spacing: Kirigami.Units.smallSpacing

    readonly property bool ready: ledSync.support === "ready"
    readonly property bool audioMode: ledSync.mode === "spectrum" || ledSync.mode === "ambient+pulse"
    readonly property var modes: [
        { value: "ambient", text: qsTr("Ambient"),
          description: qsTr("Each device takes the colours of the matching part of the wallpaper.") },
        { value: "spectrum", text: qsTr("Spectrum"),
          description: qsTr("Devices show the sound you are playing as a moving spectrum. Needs audio.") },
        { value: "ambient+pulse", text: qsTr("Ambient + pulse"),
          description: qsTr("Wallpaper colours that brighten with the bass. Needs audio.") }
    ]
    readonly property var info: stateInfo()
    // Controllers OpenRGB reported minus the listed ones the user excluded
    // (ids of devices that are not connected right now do not count).
    readonly property int syncedCount: {
        // Exact count from the worker; the estimate below only serves an
        // older worker that does not report it.
        if (ledSync.drivenCount >= 0)
            return ledSync.drivenCount;
        const disabled = ledSync.disabledDevices;
        let excluded = 0;
        for (const device of ledSync.devices) {
            if (disabled.indexOf(device.id) >= 0)
                ++excluded;
        }
        return Math.max(0, Math.max(ledSync.deviceCount, ledSync.devices.length) - excluded);
    }

    function degradedText(reason) {
        switch (reason) {
        case "timeout": return qsTr("OpenRGB stopped answering in time.");
        case "disconnected": return qsTr("The connection to OpenRGB was lost.");
        case "io_error": return qsTr("A connection error occurred while talking to OpenRGB.");
        case "protocol_error": return qsTr("OpenRGB sent an answer LED sync could not understand.");
        case "unsupported_version": return qsTr("This OpenRGB version uses an SDK protocol LED sync does not support.");
        case "device_error": return qsTr("A device did not accept a colour update.");
        case "worker_unresponsive": return qsTr("The LED sync helper stopped responding and will be restarted.");
        case "error": return qsTr("An unexpected error occurred.");
        default: return reason === "" ? qsTr("No reason was given.") : qsTr("Reason: %1.").arg(reason);
        }
    }

    // {icon, title, detail, busy, checkAgain} for the daemon state. Every
    // state has text and an icon, never colour alone.
    function stateInfo() {
        const reason = ledSync.reason;
        switch (ledSync.state) {
        case "disabled":
            return { icon: "media-playback-stop", busy: false, checkAgain: false,
                     title: qsTr("LED sync is off"),
                     detail: qsTr("Turn it on to light up OpenRGB devices with the wallpaper.") };
        case "worker_unavailable":
            if (reason === "not_installed")
                return { icon: "dialog-warning", busy: false, checkAgain: true,
                         title: qsTr("The LED sync helper is missing"),
                         detail: qsTr("kwe-led-worker was not found next to the wallpaper service. Reinstalling KDE Wallpaper Engine restores it.") };
            if (reason === "restart_budget_exhausted")
                return { icon: "dialog-warning", busy: false, checkAgain: true,
                         title: qsTr("The LED sync helper kept stopping"),
                         detail: qsTr("It stopped unexpectedly several times in a few minutes and was paused. Check again to start it once more.") };
            return { icon: "dialog-information", busy: false, checkAgain: false,
                     title: qsTr("LED sync is not available in this session"),
                     detail: qsTr("The wallpaper service was started without its LED sync helper (--no-led-worker).") };
        case "starting":
            return { icon: "", busy: true, checkAgain: false,
                     title: reason === "restarting" ? qsTr("Restarting the LED sync helper…")
                          : reason === "worker_stopping" ? qsTr("Restoring your lighting before starting again…")
                          : qsTr("Starting LED sync…"),
                     detail: "" };
        case "connecting":
            return { icon: "", busy: true, checkAgain: false,
                     title: qsTr("Looking for OpenRGB…"), detail: "" };
        case "server_absent":
            return { icon: "dialog-information", busy: false, checkAgain: true,
                     title: qsTr("OpenRGB's SDK server is not running"),
                     detail: qsTr("LED sync is optional. To use it, open OpenRGB and choose SDK Server → Start Server, or start OpenRGB with “openrgb --server”. LED sync connects by itself once the server is running.") };
        case "active":
            if (reason === "waiting_for_source")
                return { icon: "dialog-information", busy: false, checkAgain: false,
                         title: ledSync.mode === "spectrum" ? qsTr("Connected to OpenRGB — waiting for audio")
                                                            : qsTr("Connected to OpenRGB — waiting for a wallpaper"),
                         detail: (ledSync.mode === "spectrum"
                                  ? qsTr("Spectrum mode lights your devices once audio arrives.")
                                  : qsTr("Your devices keep their own lighting until a wallpaper is showing."))
                             + " " + (ledSync.deviceCount === 1 ? qsTr("1 device found.")
                                      : qsTr("%1 devices found.").arg(ledSync.deviceCount)) };
            if (ledSync.deviceCount === 0)
                return { icon: "dialog-information", busy: false, checkAgain: false,
                         title: qsTr("Connected to OpenRGB, but it reports no devices"),
                         detail: qsTr("Check that OpenRGB lists your devices on its Devices tab.") };
            return { icon: "emblem-success", busy: false, checkAgain: false,
                     title: section.syncedCount === 1 ? qsTr("Syncing 1 device with OpenRGB")
                          : qsTr("Syncing %1 devices with OpenRGB").arg(section.syncedCount),
                     detail: (ledSync.deviceCount === 1 ? qsTr("1 device found.")
                              : qsTr("%1 devices found.").arg(ledSync.deviceCount))
                         + (ledSync.protocolVersion > 0
                              ? " " + qsTr("OpenRGB SDK protocol version %1.").arg(ledSync.protocolVersion)
                              : "") };
        case "off":
            if (reason === "paused" || ledSync.paused)
                return { icon: "media-playback-pause", busy: false, checkAgain: false,
                         title: qsTr("Paused"),
                         detail: qsTr("Paused while a fullscreen app covers the desktop or the displays are off. Your devices show their own lighting until the wallpaper resumes.") };
            return { icon: "media-playback-stop", busy: false, checkAgain: false,
                     title: qsTr("Mode is Off"),
                     detail: qsTr("Connected to OpenRGB; your devices keep their own lighting. Choose a mode to sync them.") };
        case "degraded":
            return { icon: "dialog-warning", busy: false, checkAgain: true,
                     title: qsTr("LED sync is having trouble"),
                     detail: qsTr("%1 It keeps retrying by itself.").arg(degradedText(reason)) };
        default:
            return { icon: "dialog-question", busy: false, checkAgain: false,
                     title: qsTr("LED sync reported an unfamiliar state"),
                     detail: qsTr("State “%1”. A newer version of this app may describe it.").arg(ledSync.state) };
        }
    }

    Kirigami.Heading {
        text: qsTr("LED sync")
        level: 2
    }

    Controls.Label {
        Layout.fillWidth: true
        text: qsTr("Lights up keyboards, fans and LED strips with the colours of your wallpaper through OpenRGB. Optional: nothing changes until OpenRGB's SDK server is running.")
        wrapMode: Text.Wrap
        opacity: 0.8
    }

    // Daemon-level availability (loading / older daemon / no answer).
    RowLayout {
        Layout.fillWidth: true
        visible: !section.ready
        spacing: Kirigami.Units.smallSpacing

        Controls.BusyIndicator {
            visible: ledSync.support === "loading"
            running: visible
            Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
            Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            Accessible.name: qsTr("Checking LED sync")
        }
        Kirigami.Icon {
            visible: ledSync.support !== "loading"
            source: ledSync.support === "unreachable" ? "dialog-warning" : "dialog-information"
            Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
            Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            Accessible.ignored: true
        }
        Controls.Label {
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            text: {
                switch (ledSync.support) {
                case "loading": return qsTr("Checking LED sync…");
                case "unsupported": return qsTr("LED sync needs a newer wallpaper service. Update KDE Wallpaper Engine and restart its service to use it.");
                case "unavailable": return qsTr("LED sync is unavailable because the wallpaper service is running without its supervisor.");
                default: return qsTr("The wallpaper service is not responding, so the LED sync state is unknown. It is checked again every few seconds while this page is open.");
                }
            }
        }
    }

    Controls.Switch {
        id: enableSwitch
        visible: section.ready
        text: qsTr("Sync OpenRGB lighting with the wallpaper")
        checked: ledSync.enabled
        Accessible.name: text
        Accessible.description: qsTr("When on, LED sync uses OpenRGB whenever its SDK server is running. When off, your devices keep their own lighting.")
        Controls.ToolTip.visible: hovered
        Controls.ToolTip.text: Accessible.description
        onToggled: ledSync.setEnabled(checked)
    }

    // State line: icon + title + detail, and "Check again" where it helps.
    RowLayout {
        Layout.fillWidth: true
        visible: section.ready
        spacing: Kirigami.Units.largeSpacing

        Item {
            Layout.alignment: Qt.AlignTop
            Layout.preferredWidth: Kirigami.Units.iconSizes.medium
            Layout.preferredHeight: Kirigami.Units.iconSizes.medium

            Controls.BusyIndicator {
                anchors.fill: parent
                visible: section.info.busy
                running: visible
                Accessible.name: section.info.title
            }
            Kirigami.Icon {
                anchors.fill: parent
                visible: !section.info.busy
                source: section.info.icon
                Accessible.ignored: true
            }
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            Controls.Label {
                id: stateTitle
                Layout.fillWidth: true
                text: section.info.title
                font.bold: true
                wrapMode: Text.Wrap
                Accessible.role: Accessible.StaticText
                Accessible.name: qsTr("LED sync status: %1").arg(text)
            }
            Controls.Label {
                Layout.fillWidth: true
                visible: text !== ""
                text: section.info.detail
                wrapMode: Text.Wrap
            }
        }

        Controls.Button {
            Layout.alignment: Qt.AlignTop
            visible: section.info.checkAgain
            enabled: ledSync.canReprobe
            icon.name: "view-refresh"
            text: qsTr("Check again")
            Accessible.description: qsTr("Asks LED sync to look for OpenRGB's SDK server now, or to start its helper again.")
            Controls.ToolTip.visible: hovered
            Controls.ToolTip.text: Accessible.description
            onClicked: ledSync.reprobe()
        }
    }

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        type: Kirigami.MessageType.Information
        visible: section.ready && ledSync.enabled && section.audioMode && ledSync.hasWorkerStatus
                 && !ledSync.audioFresh && (ledSync.state === "active" || ledSync.state === "degraded")
        text: qsTr("No audio is reaching LED sync right now. Audio capture may be off, or no wallpaper is running. Until audio arrives, Spectrum stays dark and Ambient + pulse shows steady wallpaper colours.")
    }

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        type: Kirigami.MessageType.Warning
        visible: section.ready && ledSync.errorMessage !== ""
        text: ledSync.errorMessage
    }

    Kirigami.FormLayout {
        Layout.fillWidth: true
        visible: section.ready
        enabled: ledSync.enabled

        ColumnLayout {
            Kirigami.FormData.label: qsTr("Mode:")
            spacing: Kirigami.Units.smallSpacing

            Controls.ComboBox {
                id: modeBox
                textRole: "text"
                valueRole: "value"
                model: section.modes
                currentIndex: indexOfValue(ledSync.mode)
                displayText: currentIndex < 0 ? qsTr("Off") : currentText
                Accessible.name: qsTr("LED sync mode")
                Accessible.description: currentIndex < 0
                    ? qsTr("Off: devices keep their own lighting. Choose a mode to sync them.")
                    : section.modes[currentIndex].description
                onActivated: ledSync.setMode(currentValue)
            }
            Controls.Label {
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                text: modeBox.Accessible.description
                wrapMode: Text.Wrap
                opacity: 0.8
            }
        }

        RowLayout {
            Kirigami.FormData.label: qsTr("Brightness:")
            spacing: Kirigami.Units.smallSpacing

            Controls.Slider {
                id: brightnessSlider
                from: 0
                to: 100
                stepSize: 5
                snapMode: Controls.Slider.SnapAlways
                value: ledSync.brightness
                Layout.preferredWidth: Kirigami.Units.gridUnit * 12
                Accessible.name: qsTr("LED brightness")
                Accessible.description: qsTr("Upper limit for LED brightness, from 0 to 100 percent.")
                onMoved: ledSync.setBrightness(Math.round(value))
            }
            Controls.Label {
                text: qsTr("%1 %").arg(Math.round(brightnessSlider.value))
                Layout.minimumWidth: Kirigami.Units.gridUnit * 3
                Accessible.ignored: true
            }
        }

        Controls.SpinBox {
            Kirigami.FormData.label: qsTr("Update rate:")
            from: 1
            to: 20
            value: ledSync.rateHz
            editable: true
            textFromValue: function(value) { return qsTr("%1 Hz").arg(value); }
            valueFromText: function(text) { return parseInt(text, 10); }
            Accessible.name: qsTr("LED update rate")
            Accessible.description: qsTr("How many times per second the lights change, from 1 to 20. Some devices stutter at high rates; 10 is a good default.")
            Controls.ToolTip.visible: hovered
            Controls.ToolTip.text: Accessible.description
            onValueModified: ledSync.setRateHz(value)
        }
    }

    Kirigami.Heading {
        visible: deviceList.visible
        text: qsTr("Devices")
        level: 3
    }

    ColumnLayout {
        id: deviceList
        Layout.fillWidth: true
        visible: section.ready && ledSync.hasWorkerStatus && ledSync.devices.length > 0
        enabled: ledSync.enabled
        spacing: 0

        Repeater {
            model: ledSync.devices

            delegate: Controls.CheckBox {
                id: deviceBox

                required property var modelData

                Layout.fillWidth: true
                readonly property string details: {
                    const count = deviceBox.modelData.leds;
                    const leds = count === 1 ? qsTr("1 LED") : qsTr("%1 LEDs").arg(count);
                    return deviceBox.modelData.vendor !== ""
                        ? qsTr("%1 · %2").arg(deviceBox.modelData.vendor).arg(leds)
                        : leds;
                }
                text: qsTr("%1 (%2)").arg(deviceBox.modelData.name).arg(deviceBox.details)
                checked: ledSync.disabledDevices.indexOf(deviceBox.modelData.id) < 0
                Accessible.name: deviceBox.modelData.name
                Accessible.description: qsTr("%1. When unchecked, LED sync leaves this device's lighting alone.").arg(deviceBox.details)
                onToggled: ledSync.setDeviceEnabled(deviceBox.modelData.id, checked)
            }
        }

        Controls.Label {
            Layout.fillWidth: true
            visible: ledSync.devicesTruncated
            text: qsTr("Only the first %1 of %2 devices are listed. Devices not shown here stay synced.")
                .arg(ledSync.devices.length).arg(Math.max(ledSync.deviceCount, ledSync.devices.length))
            wrapMode: Text.Wrap
            opacity: 0.8
        }
    }
}
