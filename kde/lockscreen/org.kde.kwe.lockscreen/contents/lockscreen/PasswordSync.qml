// SPDX-License-Identifier: GPL-3.0-or-later
// One password buffer shared by the greeter's per-screen views (they share
// a QML engine), so typing on any screen fills every terminal.
pragma Singleton

import QtQuick

QtObject {
    property string password
}
