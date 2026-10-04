// SPDX-License-Identifier: GPL-3.0-or-later
// Root object kscreenlocker_greet loads from a Plasma/Shell package
// ("lockscreenmainscript"). The properties and signals below are the
// greeter's interface (interface version 2); the UI lives in LockScreenUi.
import QtQuick

Item {
    id: root

    property bool debug: false
    property string notification
    signal clearPassword()
    signal notificationRepeated()

    // Set by the greeter: true once this screen's view is shown.
    property bool viewVisible: false

    implicitWidth: 800
    implicitHeight: 600

    LockScreenUi {
        anchors.fill: parent
    }
}
