// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// Playback control and entry editor for the playlist selected in the gallery
// toolbar. The daemon owns the playback session (playlist.activate/status);
// entry edits go through playlistController, which persists them via
// playlist.put and re-syncs from the daemon's authoritative list.
ColumnLayout {
    id: panel

    required property string playlistName

    // entries() is a plain invokable, so the list is re-read explicitly on
    // every controller change instead of relying on a binding dependency
    // that does not exist.
    property var entryIds: []
    property bool shuffleOn: false
    property bool entriesVisible: false
    property bool confirmDeleteOpen: false

    readonly property bool isActive: playlistController.playing
        && playlistController.activeName === panel.playlistName

    function reload() {
        panel.entryIds = panel.playlistName !== ""
            ? playlistController.entries(panel.playlistName) : [];
        panel.shuffleOn = panel.playlistName !== ""
            && playlistController.shuffle(panel.playlistName);
    }

    function formatRemaining(seconds) {
        if (seconds < 0)
            return "";
        const minutes = Math.floor(seconds / 60);
        return minutes > 0
            ? qsTr("%1m %2s").arg(minutes).arg(seconds % 60)
            : qsTr("%1s").arg(seconds);
    }

    onPlaylistNameChanged: panel.reload()
    Component.onCompleted: panel.reload()

    Connections {
        target: playlistController
        function onChanged() { panel.reload() }
    }

    spacing: Kirigami.Units.smallSpacing

    RowLayout {
        Layout.fillWidth: true
        spacing: Kirigami.Units.smallSpacing

        Controls.Label {
            text: panel.playlistName
            font.bold: true
            elide: Text.ElideRight
        }
        Controls.ToolButton {
            visible: !panel.isActive
            text: qsTr("Play")
            icon.name: "media-playback-start-symbolic"
            display: Controls.AbstractButton.TextBesideIcon
            enabled: panel.entryIds.length > 0
            Accessible.name: qsTr("Play this playlist")
            Accessible.description: qsTr("The wallpaper service switches the wallpaper on the playlist's schedule")
            onClicked: playlistController.play(panel.playlistName)
        }
        Controls.ToolButton {
            visible: panel.isActive
            text: qsTr("Stop")
            icon.name: "media-playback-stop-symbolic"
            display: Controls.AbstractButton.TextBesideIcon
            Accessible.name: qsTr("Stop playlist playback")
            onClicked: playlistController.stop()
        }
        Controls.Label {
            Layout.fillWidth: true
            elide: Text.ElideRight
            opacity: 0.85
            text: {
                if (!playlistController.playing)
                    return qsTr("Not playing");
                const active = playlistController.activeName;
                if (playlistController.playbackState === "no_eligible")
                    return qsTr("Playing %1 — no entry is available to show").arg(active);
                if (playlistController.playbackState === "exhausted")
                    return qsTr("Playing %1 — finished (repeat is off)").arg(active);
                const current = catalogStats.itemById(playlistController.nowPlayingId).title;
                if (playlistController.playbackState === "paused")
                    return qsTr("Playing %1 — paused on %2").arg(active).arg(current);
                const remaining = panel.formatRemaining(playlistController.remainingSeconds);
                return remaining !== ""
                    ? qsTr("Playing %1 — showing %2, next change in %3")
                        .arg(active).arg(current).arg(remaining)
                    : qsTr("Playing %1 — showing %2").arg(active).arg(current);
            }
        }
        Controls.ToolButton {
            text: panel.entriesVisible
                ? qsTr("Hide entries") : qsTr("Show %1 entries").arg(panel.entryIds.length)
            icon.name: panel.entriesVisible ? "go-up-symbolic" : "go-down-symbolic"
            display: Controls.AbstractButton.TextBesideIcon
            Accessible.name: panel.entriesVisible
                ? qsTr("Hide the playlist entries") : qsTr("Show the playlist entries")
            onClicked: panel.entriesVisible = !panel.entriesVisible
        }
        Controls.ToolButton {
            icon.name: "edit-delete-symbolic"
            text: qsTr("Delete playlist")
            display: Controls.AbstractButton.IconOnly
            Accessible.name: qsTr("Delete this playlist")
            Controls.ToolTip.visible: hovered
            Controls.ToolTip.text: text
            onClicked: panel.confirmDeleteOpen = true
        }
    }

    Controls.Label {
        Layout.fillWidth: true
        visible: panel.isActive && playlistController.unavailableIds.length > 0
        text: qsTr("%1 entries are skipped because they are not installed, were quarantined, or need a missing capability.")
            .arg(playlistController.unavailableIds.length)
        wrapMode: Text.Wrap
        opacity: 0.75
    }

    ListView {
        id: entryList

        Layout.fillWidth: true
        Layout.preferredHeight: Math.min(contentHeight, Kirigami.Units.gridUnit * 14)
        visible: panel.entriesVisible && panel.entryIds.length > 0
        clip: true
        model: panel.entryIds
        Accessible.name: qsTr("Playlist entries in playback order")

        Controls.ScrollBar.vertical: Controls.ScrollBar {}

        delegate: RowLayout {
            id: entryRow

            required property int index
            required property string modelData

            readonly property var item: catalogStats.itemById(entryRow.modelData)

            width: entryList.width - Kirigami.Units.largeSpacing
            spacing: Kirigami.Units.smallSpacing

            Controls.Label {
                text: entryRow.index + 1
                opacity: 0.6
                Layout.minimumWidth: Kirigami.Units.gridUnit * 1.4
                horizontalAlignment: Text.AlignRight
            }
            Image {
                source: entryRow.item.previewUrl
                sourceSize.width: Kirigami.Units.gridUnit * 3
                sourceSize.height: Kirigami.Units.gridUnit * 2
                Layout.preferredWidth: Kirigami.Units.gridUnit * 3
                Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                fillMode: Image.PreserveAspectCrop
                asynchronous: true
            }
            Kirigami.Icon {
                visible: panel.isActive && playlistController.nowPlayingId === entryRow.modelData
                source: "media-playback-start-symbolic"
                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                Accessible.name: qsTr("Currently showing")
            }
            ColumnLayout {
                Layout.fillWidth: true
                spacing: 0
                Controls.Label {
                    Layout.fillWidth: true
                    text: entryRow.item.title
                    elide: Text.ElideRight
                }
                Controls.Label {
                    Layout.fillWidth: true
                    text: entryRow.item.found
                        ? entryRow.item.kind : qsTr("not installed")
                    opacity: 0.6
                    font.pointSize: Kirigami.Theme.smallFont.pointSize
                    elide: Text.ElideRight
                }
            }
            Controls.ToolButton {
                icon.name: "go-up-symbolic"
                enabled: entryRow.index > 0
                Accessible.name: qsTr("Move %1 up").arg(entryRow.item.title)
                onClicked: playlistController.moveEntry(panel.playlistName,
                    entryRow.index, entryRow.index - 1)
            }
            Controls.ToolButton {
                icon.name: "go-down-symbolic"
                enabled: entryRow.index < panel.entryIds.length - 1
                Accessible.name: qsTr("Move %1 down").arg(entryRow.item.title)
                onClicked: playlistController.moveEntry(panel.playlistName,
                    entryRow.index, entryRow.index + 1)
            }
            Controls.ToolButton {
                icon.name: "list-remove-symbolic"
                Accessible.name: qsTr("Remove %1 from the playlist").arg(entryRow.item.title)
                onClicked: playlistController.removeEntry(panel.playlistName, entryRow.modelData)
            }
        }
    }

    Controls.Label {
        visible: panel.entriesVisible && panel.entryIds.length === 0
        text: qsTr("No entries yet. Select a wallpaper and use “Add to %1” in the details pane.")
            .arg(panel.playlistName)
        wrapMode: Text.Wrap
        Layout.fillWidth: true
        opacity: 0.75
    }

    Controls.Label {
        visible: panel.entriesVisible && panel.entryIds.length > 1 && panel.shuffleOn
        text: qsTr("Shuffle is on; the order below is ignored until it is turned off.")
        wrapMode: Text.Wrap
        Layout.fillWidth: true
        opacity: 0.6
    }

    Controls.Dialog {
        id: confirmDeleteDialog

        modal: true
        title: qsTr("Delete playlist")
        visible: panel.confirmDeleteOpen
        standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
        anchors.centerIn: Controls.Overlay.overlay
        onAccepted: {
            panel.confirmDeleteOpen = false;
            playlistController.remove(panel.playlistName);
        }
        onRejected: panel.confirmDeleteOpen = false

        contentItem: Controls.Label {
            text: qsTr("Delete “%1”? The wallpapers themselves are not removed.")
                .arg(panel.playlistName)
            wrapMode: Text.Wrap
        }
    }
}
