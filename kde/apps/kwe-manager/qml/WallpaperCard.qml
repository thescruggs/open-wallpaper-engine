// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// Gallery card shared by the Installed and Workshop views. Consumes the
// CatalogModel roles directly; selection goes through WallpaperSelection so
// both views present the same details pane.
Item {
    id: cardRoot

    required property real cellWidth
    required property real cellHeight
    required property bool workshopView

    required property string title
    required property string workshopId
    required property string kind
    required property string compatibility
    required property string compatibilityDetail
    required property url previewUrl
    required property int diagnosticCount
    required property string workshopState
    required property int workshopProgress
    // W5: Steam lists a newer version than the installed one (display only).
    required property bool workshopUpdateAvailable
    required property bool favorite
    required property var tags
    required property url entryUrl
    required property url contentRoot
    required property string diagnosticSummary
    required property var requestedPermissions

    // Playlist edit mode (GalleryPage): the card shows a checkbox and a click
    // adds the wallpaper to, or removes it from, the playlist being edited.
    property bool selectionMode: false
    property var playlistMembers: ({})
    readonly property bool inPlaylist: playlistMembers[workshopId] === true
    // Same rule the daemon uses for what a playlist can play.
    readonly property bool playlistSelectable: (workshopState === "local" || workshopState === "subscribed_installed")
        && kind !== "invalid"
    signal playlistToggleRequested(string workshopId, bool extendRange)

    width: cardRoot.cellWidth
    height: cardRoot.cellHeight

    Controls.AbstractButton {
        id: card
        anchors.fill: parent
        anchors.margins: Kirigami.Units.smallSpacing
        hoverEnabled: true
        activeFocusOnTab: true
        // Qt 6.11: multi-argument String.arg() throws; chain single-arg calls.
        Accessible.name: (cardRoot.workshopUpdateAvailable
                ? qsTr("%1, %2 wallpaper, %3, update available")
                : qsTr("%1, %2 wallpaper, %3"))
            .arg(cardRoot.title).arg(cardRoot.kind)
            .arg(WallpaperSelection.compatibilityLabel(cardRoot.compatibility))
        Accessible.description: !cardRoot.selectionMode ? ""
            : !cardRoot.playlistSelectable ? qsTr("Cannot be added to a playlist")
            : cardRoot.inPlaylist ? qsTr("In the playlist") : qsTr("Not in the playlist")
        Accessible.checkable: cardRoot.selectionMode
        Accessible.checked: cardRoot.selectionMode && cardRoot.inPlaylist
        // Keyboard activation lands here in both modes; pointer clicks in
        // selection mode go through the overlay below so Shift is known.
        onClicked: cardRoot.selectionMode
            ? (cardRoot.playlistSelectable ? cardRoot.playlistToggleRequested(cardRoot.workshopId, false) : undefined)
            : WallpaperSelection.select(cardRoot.title, cardRoot.workshopId, cardRoot.kind, cardRoot.compatibility, cardRoot.compatibilityDetail, cardRoot.previewUrl, cardRoot.tags, cardRoot.entryUrl, cardRoot.contentRoot, cardRoot.diagnosticSummary, cardRoot.requestedPermissions)

        background: Rectangle {
            radius: Kirigami.Units.cornerRadius
            color: card.down || card.checked ? Kirigami.Theme.alternateBackgroundColor
                 : card.hovered ? Kirigami.Theme.hoverColor
                 : Kirigami.Theme.backgroundColor
            border.width: card.activeFocus || (cardRoot.selectionMode && cardRoot.inPlaylist) ? 2 : 1
            // Kirigami 6.28 no longer provides Theme.separatorColor; tint the
            // text color instead for the idle 1px card border.
            border.color: card.activeFocus ? Kirigami.Theme.focusColor
                : cardRoot.selectionMode && cardRoot.inPlaylist ? Kirigami.Theme.highlightColor
                : Qt.rgba(Kirigami.Theme.textColor.r, Kirigami.Theme.textColor.g,
                          Kirigami.Theme.textColor.b, 0.2)
        }

        contentItem: ColumnLayout {
            spacing: Kirigami.Units.smallSpacing

            Item {
                Layout.fillWidth: true
                Layout.fillHeight: true
                Layout.minimumHeight: Kirigami.Units.gridUnit * 6

                Image {
                    id: previewImage
                    anchors.fill: parent
                    source: cardRoot.previewUrl
                    fillMode: Image.PreserveAspectCrop
                    asynchronous: true
                    cache: true
                    sourceSize.width: 384
                    sourceSize.height: 216
                    visible: status === Image.Ready
                }
                Kirigami.Icon {
                    anchors.centerIn: parent
                    width: Kirigami.Units.iconSizes.huge
                    height: width
                    source: cardRoot.kind === "video" ? "video-x-generic-symbolic"
                          : cardRoot.kind === "web" ? "internet-web-browser-symbolic"
                          : "preferences-desktop-wallpaper-symbolic"
                    opacity: 0.55
                    visible: previewImage.status !== Image.Ready
                }
                // Membership marker. Display only: the overlay MouseArea
                // takes the click for the whole card.
                Rectangle {
                    visible: cardRoot.selectionMode
                    anchors.top: parent.top
                    anchors.left: parent.left
                    anchors.margins: Kirigami.Units.smallSpacing
                    width: membershipBox.implicitWidth + Kirigami.Units.smallSpacing
                    height: membershipBox.implicitHeight + Kirigami.Units.smallSpacing
                    radius: Kirigami.Units.cornerRadius
                    color: Kirigami.Theme.backgroundColor
                    opacity: cardRoot.playlistSelectable ? 0.92 : 0.6
                    Controls.CheckBox {
                        id: membershipBox
                        anchors.centerIn: parent
                        checked: cardRoot.inPlaylist
                        enabled: cardRoot.playlistSelectable
                        focusPolicy: Qt.NoFocus
                        Accessible.ignored: true
                    }
                }
            }

            Kirigami.Heading {
                Layout.fillWidth: true
                level: 4
                text: cardRoot.title
                elide: Text.ElideRight
                maximumLineCount: 1
            }
            RowLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing
                Kirigami.Icon {
                    width: Kirigami.Units.iconSizes.small
                    height: width
                    source: WallpaperSelection.compatibilityIcon(cardRoot.compatibility)
                }
                Controls.Label {
                    Layout.fillWidth: true
                    text: WallpaperSelection.compatibilityLabel(cardRoot.compatibility)
                    elide: Text.ElideRight
                }
                Controls.ToolButton {
                    Layout.alignment: Qt.AlignRight
                    icon.name: cardRoot.favorite ? "starred-symbolic" : "non-starred-symbolic"
                    Accessible.name: cardRoot.favorite ? qsTr("Remove from favorites") : qsTr("Add to favorites")
                    Controls.ToolTip.visible: hovered
                    Controls.ToolTip.text: Accessible.name
                    onClicked: {
                        catalogStats.toggleFavorite(cardRoot.workshopId)
                    }
                }
                Controls.Label {
                    visible: cardRoot.diagnosticCount > 0
                    text: qsTr("%1 issue(s)").arg(cardRoot.diagnosticCount)
                    Accessible.name: text
                }
                Controls.Label {
                    visible: cardRoot.workshopState === "subscribed_missing"
                    text: qsTr("Awaiting Steam download")
                    color: Kirigami.Theme.neutralTextColor
                }
                Controls.Label {
                    visible: cardRoot.workshopState === "downloading"
                    text: cardRoot.workshopProgress >= 0
                        ? qsTr("Downloading · %1%").arg(cardRoot.workshopProgress)
                        : qsTr("Downloading")
                    color: Kirigami.Theme.neutralTextColor
                }
                Controls.Label {
                    visible: cardRoot.workshopView && cardRoot.workshopState === "subscribed_installed"
                        && !cardRoot.workshopUpdateAvailable
                    text: qsTr("Installed")
                    color: Kirigami.Theme.positiveTextColor
                }
                // Icon and text, never colour alone (UX_DESIGN.md).
                Kirigami.Icon {
                    visible: cardRoot.workshopUpdateAvailable
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    source: "update-low-symbolic"
                    Accessible.ignored: true
                }
                Controls.Label {
                    visible: cardRoot.workshopUpdateAvailable
                    text: qsTr("Update available")
                    color: Kirigami.Theme.neutralTextColor
                }
            }
        }
    }
    MouseArea {
        anchors.fill: card
        enabled: cardRoot.selectionMode
        visible: enabled
        cursorShape: cardRoot.playlistSelectable ? Qt.PointingHandCursor : Qt.ForbiddenCursor
        onClicked: mouse => {
            card.forceActiveFocus()
            if (cardRoot.playlistSelectable)
                cardRoot.playlistToggleRequested(cardRoot.workshopId, (mouse.modifiers & Qt.ShiftModifier) !== 0)
        }
    }
}
