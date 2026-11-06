// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Kirigami.ApplicationWindow {
    id: root
    width: 1220
    height: 760
    minimumWidth: 760
    minimumHeight: 520
    visible: true
    title: qsTr("KDE Wallpaper Engine")

    property bool createPlaylistOpen: false
    // The gallery that asked for the new playlist; it enters edit mode on it.
    property var createPlaylistFor: null

    pageStack.initialPage: installedPage

    // --start-page: installed (default), workshop, settings, or led
    // (Settings scrolled to LED sync). Used for screenshots and smokes.
    Component.onCompleted: {
        if (startPage === "workshop" || startPage === "settings" || startPage === "led") {
            while (pageStack.depth > 0)
                pageStack.pop()
            pageStack.push(startPage === "workshop" ? workshopPage : settingsPage)
            if (startPage === "led")
                ledScrollTimer.start()
        }
    }

    // The section's position is known once the page has been laid out.
    Timer {
        id: ledScrollTimer
        interval: 1500
        onTriggered: settingsPage.showLedSync()
    }

    Kirigami.Page {
        id: installedPage
        visible: false
        title: qsTr("Installed Wallpapers")
        contentItem: GalleryPage {
            id: installedGallery
            anchors.fill: parent
            filterModel: wallpaperModel
            workshopView: false
            emptyExplanation: qsTr("Change the search or type filter, or ask Steam to install Workshop items.")
            onCreatePlaylistRequested: {
                root.createPlaylistFor = installedGallery
                root.createPlaylistOpen = true
            }
        }
    }

    Kirigami.Page {
        id: workshopPage
        visible: false
        title: qsTr("Workshop")

        // Two views of the Workshop: what is subscribed locally (works
        // offline) and the live Steam Workshop browser.
        header: Controls.TabBar {
            id: workshopTabs
            Controls.TabButton {
                text: qsTr("Subscribed")
                icon.name: "folder-download-symbolic"
                display: Controls.AbstractButton.TextBesideIcon
            }
            Controls.TabButton {
                text: qsTr("Browse")
                icon.name: "internet-services-symbolic"
                display: Controls.AbstractButton.TextBesideIcon
            }
        }

        contentItem: StackLayout {
            currentIndex: workshopTabs.currentIndex

            GalleryPage {
                id: subscribedGallery
                filterModel: workshopModel
                workshopView: true
                emptyExplanation: qsTr("No subscribed Workshop items are installed or downloading. Subscribe in Steam, then rescan here to see download progress.")
                onCreatePlaylistRequested: {
                    root.createPlaylistFor = subscribedGallery
                    root.createPlaylistOpen = true
                }
            }

            WorkshopBrowsePage {
                active: workshopPage.visible && workshopTabs.currentIndex === 1
            }
        }
    }

    SettingsPage {
        id: settingsPage
        visible: false
    }

    footer: Kirigami.NavigationTabBar {
        actions: [
            Kirigami.Action {
                icon.name: "preferences-desktop-wallpaper-symbolic"
                text: qsTr("Installed")
                checked: installedPage.visible
                onTriggered: {
                    if (!installedPage.visible) {
                        while (pageStack.depth > 0)
                            pageStack.pop()
                        pageStack.push(installedPage)
                    }
                }
            },
            Kirigami.Action {
                icon.name: "folder-download-symbolic"
                // W5: the subscribed count rides in the label (the same
                // number as the Subscribed view's heading), so it is read
                // out with the tab and needs no newer-Kirigami badge type.
                text: catalogStats.subscribedCount > 0
                    ? qsTr("Workshop (%1)").arg(catalogStats.subscribedCount)
                    : qsTr("Workshop")
                tooltip: catalogStats.updateAvailableCount > 0
                    ? qsTr("%1 subscribed, %2 with an update available")
                        .arg(catalogStats.subscribedCount).arg(catalogStats.updateAvailableCount)
                    : qsTr("%1 subscribed").arg(catalogStats.subscribedCount)
                checked: workshopPage.visible
                onTriggered: {
                    if (!workshopPage.visible) {
                        while (pageStack.depth > 0)
                            pageStack.pop()
                        pageStack.push(workshopPage)
                    }
                }
            },
            Kirigami.Action {
                icon.name: "configure-symbolic"
                text: qsTr("Settings")
                checked: settingsPage.visible
                onTriggered: {
                    if (!settingsPage.visible) {
                        while (pageStack.depth > 0)
                            pageStack.pop()
                        pageStack.push(settingsPage)
                    }
                }
            }
        ]
    }

    Controls.Dialog {
        id: createPlaylistDialog
        modal: true
        title: qsTr("Create playlist")
        visible: root.createPlaylistOpen
        standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
        onAccepted: {
            const name = nameField.text.trim()
            playlistController.create(name)
            nameField.clear()
            root.createPlaylistOpen = false
            if (root.createPlaylistFor !== null)
                root.createPlaylistFor.beginEditing(name)
        }
        onRejected: root.createPlaylistOpen = false
        contentItem: Controls.TextField {
            id: nameField
            placeholderText: qsTr("Playlist name")
            Accessible.name: qsTr("New playlist name")
        }
    }
}
