// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kwe

// Steam account card for the Browse tab. Every state carries an icon and
// text (never color alone). Sign-in is a QR code scanned with the Steam
// mobile app; the manager never handles the password or a token.
Kirigami.AbstractCard {
    id: card

    readonly property int authState: workshopClient.authState
    readonly property bool pending: authState === WorkshopClient.Starting
        || authState === WorkshopClient.AwaitingScan
        || authState === WorkshopClient.AwaitingConfirmation

    contentItem: ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.largeSpacing

            Kirigami.Icon {
                Layout.alignment: Qt.AlignTop
                implicitWidth: Kirigami.Units.iconSizes.large
                implicitHeight: implicitWidth
                source: card.authState === WorkshopClient.SignedIn ? "user-online-symbolic"
                      : card.authState === WorkshopClient.WorkerUnavailable ? "dialog-warning-symbolic"
                      : card.authState === WorkshopClient.Failed ? "dialog-error-symbolic"
                      : card.authState === WorkshopClient.AwaitingConfirmation ? "smartphone-symbolic"
                      : card.authState === WorkshopClient.AwaitingScan ? "view-barcode-qr-symbolic"
                      : "user-offline-symbolic"
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing

                Kirigami.Heading {
                    Layout.fillWidth: true
                    level: 3
                    wrapMode: Text.WordWrap
                    textFormat: Text.PlainText
                    text: {
                        switch (card.authState) {
                        case WorkshopClient.Starting: return qsTr("Starting Steam sign-in…")
                        case WorkshopClient.AwaitingScan: return qsTr("Scan the code with Steam")
                        case WorkshopClient.AwaitingConfirmation: return qsTr("Approve the sign-in on your phone")
                        case WorkshopClient.SignedIn: return qsTr("Signed in as %1").arg(workshopClient.accountName)
                        case WorkshopClient.WorkerUnavailable: return qsTr("Steam browsing is unavailable")
                        case WorkshopClient.Failed: return qsTr("Steam sign-in did not work")
                        case WorkshopClient.Unknown: return qsTr("Checking Steam account…")
                        default: return qsTr("Sign in to Steam to browse and subscribe")
                        }
                    }
                }

                Controls.Label {
                    Layout.fillWidth: true
                    wrapMode: Text.WordWrap
                    visible: text !== ""
                    textFormat: Text.PlainText
                    text: {
                        switch (card.authState) {
                        case WorkshopClient.AwaitingScan:
                            return qsTr("Open the Steam mobile app, tap the shield/QR icon and scan this code.")
                        case WorkshopClient.AwaitingConfirmation:
                            return qsTr("The code was scanned. Confirm the request in the Steam mobile app.")
                        case WorkshopClient.WorkerUnavailable:
                        case WorkshopClient.Failed:
                            return workshopClient.authError
                        case WorkshopClient.SignedOut:
                            return qsTr("Items you open in Steam still work without signing in.")
                        default: return ""
                        }
                    }
                }
            }
        }

        Controls.BusyIndicator {
            Layout.alignment: Qt.AlignHCenter
            visible: card.authState === WorkshopClient.Starting
            running: visible
            Accessible.name: qsTr("Starting Steam sign-in")
        }

        QrCodeItem {
            Layout.alignment: Qt.AlignHCenter
            implicitWidth: Kirigami.Units.gridUnit * 13
            implicitHeight: implicitWidth
            visible: card.authState === WorkshopClient.AwaitingScan
                && workshopClient.challengeUrl !== ""
            text: card.authState === WorkshopClient.AwaitingScan ? workshopClient.challengeUrl : ""
            Accessible.role: Accessible.Graphic
            Accessible.name: qsTr("QR code for Steam sign-in")
            Accessible.description: qsTr("Scan with the Steam mobile app to sign in")
        }

        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing

            Controls.Button {
                visible: card.authState === WorkshopClient.SignedOut
                    || card.authState === WorkshopClient.Unknown
                text: qsTr("Sign in with QR code")
                icon.name: "view-barcode-qr-symbolic"
                onClicked: workshopClient.beginSignIn()
            }
            Controls.Button {
                visible: card.authState === WorkshopClient.Failed
                text: qsTr("Try again")
                icon.name: "view-refresh-symbolic"
                onClicked: workshopClient.beginSignIn()
            }
            Controls.Button {
                visible: card.authState === WorkshopClient.WorkerUnavailable
                text: qsTr("Check again")
                icon.name: "view-refresh-symbolic"
                onClicked: workshopClient.refreshAuthStatus()
            }
            Controls.Button {
                visible: card.pending
                text: qsTr("Cancel")
                icon.name: "dialog-cancel-symbolic"
                onClicked: workshopClient.cancelSignIn()
            }
            Controls.Button {
                visible: card.authState === WorkshopClient.SignedIn
                text: qsTr("Sign out")
                icon.name: "system-log-out-symbolic"
                onClicked: workshopClient.signOut()
            }
        }

        Controls.Label {
            Layout.fillWidth: true
            wrapMode: Text.WordWrap
            font: Kirigami.Theme.smallFont
            opacity: 0.75
            text: qsTr("You can revoke this sign-in at any time from Steam's authorized-devices page (Steam account settings).")
        }
    }
}
