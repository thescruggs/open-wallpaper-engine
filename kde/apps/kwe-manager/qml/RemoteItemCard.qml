// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// Browse-grid card for a remote Workshop item. Same visual language and
// proportions as WallpaperCard; the model roles are required properties.
Item {
    id: cardRoot

    required property real cellWidth
    required property real cellHeight

    required property string workshopId
    required property string title
    required property string shortDescription
    required property url previewUrl
    required property string kind
    required property var tags
    required property real fileSize
    required property real subscriptions
    required property bool subscribed
    required property bool subscriptionPending

    signal opened(string workshopId)

    width: cardRoot.cellWidth
    height: cardRoot.cellHeight

    Controls.AbstractButton {
        id: card
        anchors.fill: parent
        anchors.margins: Kirigami.Units.smallSpacing
        hoverEnabled: true
        activeFocusOnTab: true
        Accessible.name: cardRoot.subscribed
            ? qsTr("%1, %2 wallpaper, subscribed").arg(cardRoot.title).arg(cardRoot.kind)
            : cardRoot.subscriptionPending
                ? qsTr("%1, %2 wallpaper, subscribing").arg(cardRoot.title).arg(cardRoot.kind)
                : qsTr("%1, %2 wallpaper").arg(cardRoot.title).arg(cardRoot.kind)
        onClicked: cardRoot.opened(cardRoot.workshopId)

        background: Rectangle {
            radius: Kirigami.Units.cornerRadius
            color: card.down ? Kirigami.Theme.alternateBackgroundColor
                 : card.hovered ? Kirigami.Theme.hoverColor
                 : Kirigami.Theme.backgroundColor
            border.width: card.activeFocus ? 2 : 1
            border.color: card.activeFocus ? Kirigami.Theme.focusColor
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
            }

            Kirigami.Heading {
                Layout.fillWidth: true
                level: 4
                text: cardRoot.title
                elide: Text.ElideRight
                maximumLineCount: 1
                textFormat: Text.PlainText
            }
            RowLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing

                Controls.Label {
                    Layout.fillWidth: true
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                    text: {
                        const extra = cardRoot.tags.filter(t => t.toLowerCase() !== cardRoot.kind)
                        const kindLabel = cardRoot.kind === "" ? "" : cardRoot.kind.charAt(0).toUpperCase() + cardRoot.kind.slice(1)
                        return [kindLabel].concat(extra.slice(0, 2)).filter(t => t !== "").join(" · ")
                    }
                }
                Kirigami.Icon {
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: implicitWidth
                    source: "user-symbolic"
                }
                Controls.Label {
                    text: Number(cardRoot.subscriptions).toLocaleString(Qt.locale(), "f", 0)
                    Accessible.name: qsTr("%1 subscribers").arg(text)
                }
            }
            RowLayout {
                Layout.fillWidth: true
                // Reserve the row so the grid does not shift when the badge
                // appears.
                Layout.preferredHeight: badgeLabel.implicitHeight
                spacing: Kirigami.Units.smallSpacing
                visible: true

                Kirigami.Icon {
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: implicitWidth
                    visible: cardRoot.subscribed || cardRoot.subscriptionPending
                    source: cardRoot.subscribed ? "emblem-ok-symbolic" : "view-refresh-symbolic"
                }
                Controls.Label {
                    id: badgeLabel
                    Layout.fillWidth: true
                    text: cardRoot.subscriptionPending
                        ? (cardRoot.subscribed ? qsTr("Unsubscribing…") : qsTr("Subscribing…"))
                        : cardRoot.subscribed ? qsTr("Subscribed") : ""
                    color: cardRoot.subscribed && !cardRoot.subscriptionPending
                        ? Kirigami.Theme.positiveTextColor : Kirigami.Theme.neutralTextColor
                    elide: Text.ElideRight
                }
            }
        }
    }
}
