// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// Per-renderer apply defaults. The values preselect the Apply controls in
// the details pane whenever a wallpaper of that kind is selected; each apply
// then persists its own values with the display's assignment, and playlists
// reuse that assignment on later switches.
Kirigami.ScrollablePage {
    id: settingsPage

    title: qsTr("Settings")

    readonly property var rendererKinds: [
        { kind: "scene", label: qsTr("Scene renderer") },
        { kind: "video", label: qsTr("Video renderer") },
        { kind: "web", label: qsTr("Web renderer") }
    ]

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Kirigami.Heading {
            text: qsTr("Sound")
            level: 2
        }

        Controls.Switch {
            text: qsTr("Wallpaper audio")
            checked: settingsClient.audioOutput
            enabled: settingsClient.loaded && !settingsClient.busy
            Accessible.description: qsTr("Whether wallpapers may play sound. Turning this off silences video and scene wallpapers immediately.")
            Controls.ToolTip.visible: hovered
            Controls.ToolTip.text: Accessible.description
            onToggled: settingsClient.setAudioOutput(checked)
        }

        Controls.Label {
            Layout.fillWidth: true
            text: qsTr("Applies to all wallpapers on all displays. Web wallpapers are always silent. This is separate from the per-wallpaper System audio permission, which lets audio-reactive wallpapers visualize what you are playing.")
            wrapMode: Text.Wrap
            opacity: 0.8
        }

        Kirigami.Heading {
            text: qsTr("Performance")
            level: 2
        }

        Controls.Switch {
            text: qsTr("Pause while an app is fullscreen")
            checked: settingsClient.pauseWhenCovered
            enabled: settingsClient.loaded && !settingsClient.busy
            Accessible.description: qsTr("Pauses the wallpaper while a fullscreen application, such as a game, is on every display, and resumes when it leaves fullscreen.")
            Controls.ToolTip.visible: hovered
            Controls.ToolTip.text: Accessible.description
            onToggled: settingsClient.setPauseWhenCovered(checked)
        }

        Controls.Label {
            Layout.fillWidth: true
            text: qsTr("Saves CPU and GPU while you play a game or watch a video fullscreen. Maximized windows do not pause. With more than one display the wallpaper pauses only when every display has a fullscreen app, because all displays share one renderer. Needs KWin; the last frame stays on screen while paused.")
            wrapMode: Text.Wrap
            opacity: 0.8
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            type: Kirigami.MessageType.Error
            visible: settingsClient.errorMessage !== ""
            text: settingsClient.errorMessage
        }

        Kirigami.Heading {
            text: qsTr("Renderer defaults")
            level: 2
        }

        Controls.Label {
            Layout.fillWidth: true
            text: qsTr("These defaults preselect the scaling and frame-rate controls when you apply a wallpaper of each type. Displays keep the settings they were last applied with; playlists reuse each display's last applied settings.")
            wrapMode: Text.Wrap
            opacity: 0.8
        }

        Repeater {
            model: settingsPage.rendererKinds

            delegate: Controls.GroupBox {
                id: kindBox

                required property var modelData

                Layout.fillWidth: true
                title: kindBox.modelData.label

                contentItem: Kirigami.FormLayout {
                    Controls.ComboBox {
                        Kirigami.FormData.label: qsTr("Scaling:")
                        textRole: "text"
                        valueRole: "value"
                        model: [
                            { text: qsTr("Aspect (fit, letterbox)"), value: "aspect" },
                            { text: qsTr("Fill (crop to cover)"), value: "fill" },
                            { text: qsTr("Stretch (ignore aspect)"), value: "stretch" }
                        ]
                        Accessible.name: qsTr("Default scaling mode for %1 wallpapers").arg(kindBox.modelData.kind)
                        Component.onCompleted: currentIndex =
                            indexOfValue(rendererDefaults.scaling(kindBox.modelData.kind))
                        onActivated: rendererDefaults.setScaling(kindBox.modelData.kind, currentValue)
                    }
                    Controls.ComboBox {
                        Kirigami.FormData.label: qsTr("Frame rate limit:")
                        textRole: "text"
                        valueRole: "value"
                        model: [
                            { text: qsTr("15 fps"), value: 15 },
                            { text: qsTr("24 fps"), value: 24 },
                            { text: qsTr("30 fps"), value: 30 },
                            { text: qsTr("60 fps"), value: 60 }
                        ]
                        Accessible.name: qsTr("Default frame rate limit for %1 wallpapers").arg(kindBox.modelData.kind)
                        Accessible.description: qsTr("Lower values use less CPU and GPU")
                        Component.onCompleted: {
                            const index = indexOfValue(rendererDefaults.fps(kindBox.modelData.kind));
                            currentIndex = index >= 0 ? index : indexOfValue(30);
                        }
                        onActivated: rendererDefaults.setFps(kindBox.modelData.kind, currentValue)
                    }
                }
            }
        }

        Controls.Label {
            Layout.fillWidth: true
            text: qsTr("All rendering runs on the system's active GPU; renderers inherit the compositor's device.")
            wrapMode: Text.Wrap
            opacity: 0.6
        }
    }
}
