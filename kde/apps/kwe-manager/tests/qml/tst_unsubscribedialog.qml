// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtTest
import "../../qml" as Manager

// Review fixes C2 (R9): the unsubscribe confirmation emits accepted() exactly
// once per Unsubscribe click, and Cancel / Escape emit rejected() exactly once
// without accepted() — so the details pane sends exactly one
// workshop.unsubscribe, or none. Runs offscreen under qmltestrunner (ctest).
Item {
    id: root
    width: 640
    height: 480

    property int accepts: 0
    property int rejects: 0
    // What the details pane does on accepted(): one RPC per accept.
    property var sent: []

    Manager.UnsubscribeDialog {
        id: dialog
        parent: root
        itemTitle: "Synthetic wallpaper"
        onAccepted: {
            root.accepts += 1;
            root.sent = root.sent.concat(["workshop.unsubscribe"]);
        }
        onRejected: root.rejects += 1
    }

    function findButton(item, name) {
        if (!item)
            return null;
        if (item.objectName === name)
            return item;
        const children = item.children || [];
        for (let i = 0; i < children.length; ++i) {
            const found = findButton(children[i], name);
            if (found)
                return found;
        }
        const content = item.contentChildren || [];
        for (let i = 0; i < content.length; ++i) {
            const found = findButton(content[i], name);
            if (found)
                return found;
        }
        return null;
    }

    TestCase {
        name: "UnsubscribeDialog"
        when: windowShown

        function init() {
            root.accepts = 0;
            root.rejects = 0;
            root.sent = [];
        }

        function openDialog() {
            dialog.open();
            tryCompare(dialog, "opened", true);
        }

        function test_accept_fires_once() {
            openDialog();
            const button = findButton(dialog.footer, "unsubscribeButton");
            verify(button !== null, "Unsubscribe button");
            mouseClick(button);
            tryCompare(dialog, "visible", false);
            wait(50);
            compare(root.accepts, 1, "accepted() exactly once");
            compare(root.rejects, 0);
            compare(root.sent.length, 1, "exactly one workshop.unsubscribe");
        }

        function test_cancel_fires_rejected_once_and_sends_nothing() {
            openDialog();
            const button = findButton(dialog.footer, "cancelButton");
            verify(button !== null, "Cancel button");
            mouseClick(button);
            tryCompare(dialog, "visible", false);
            wait(50);
            compare(root.rejects, 1, "rejected() exactly once");
            compare(root.accepts, 0);
            compare(root.sent.length, 0, "nothing sent");
        }

        function test_escape_fires_rejected_once_and_sends_nothing() {
            openDialog();
            keyClick(Qt.Key_Escape);
            tryCompare(dialog, "visible", false);
            wait(50);
            compare(root.rejects, 1, "rejected() exactly once");
            compare(root.accepts, 0);
            compare(root.sent.length, 0, "nothing sent");
        }
    }
}
