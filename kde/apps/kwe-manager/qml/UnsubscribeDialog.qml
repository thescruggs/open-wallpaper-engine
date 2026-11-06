// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls

// W5: unsubscribing makes Steam delete the wallpaper's files, so it is
// confirmed first. Cancel (or Escape) leaves everything untouched.
//
// The footer DialogButtonBox drives accepted()/rejected() through the
// buttons' roles alone: the Dialog already connects to the box, so an extra
// `onAccepted: dialog.accept()` on the box would emit accepted() TWICE
// (review fixes C2, R9; tests/qml/tst_unsubscribedialog.qml).
Controls.Dialog {
    id: dialog

    // The wallpaper's title, shown in the question.
    property string itemTitle: ""

    modal: true
    title: qsTr("Unsubscribe from Workshop item")
    standardButtons: Controls.Dialog.NoButton
    width: Math.min(parent ? parent.width * 0.8 : 420, 420)

    contentItem: Controls.Label {
        text: qsTr("Unsubscribe from %1? Steam removes its files from this computer; playlists that contain it skip it until you subscribe again.")
            .arg(dialog.itemTitle)
        wrapMode: Text.Wrap
    }

    footer: Controls.DialogButtonBox {
        Controls.Button {
            objectName: "cancelButton"
            text: qsTr("Cancel")
            Controls.DialogButtonBox.buttonRole: Controls.DialogButtonBox.RejectRole
        }
        Controls.Button {
            objectName: "unsubscribeButton"
            text: qsTr("Unsubscribe")
            icon.name: "list-remove-symbolic"
            Controls.DialogButtonBox.buttonRole: Controls.DialogButtonBox.AcceptRole
        }
    }
}
