// SPDX-License-Identifier: GPL-3.0-or-later
// KWE Terminal lock screen: the kwe-terminal SDDM look over the live
// wallpaper. The greeter draws the configured wallpaper plugin (for KWE,
// org.kde.kwe.wallpaper) underneath this item; nothing here renders or
// parses wallpaper content.
//
// Greeter interface used (kscreenlocker_greet, interface version 2):
// context properties authenticator, kscreenlocker_userName, config; root
// signals clearPassword/notificationRepeated. Auth flow follows the
// documented PamAuthenticators contract: startAuthenticating() on wake,
// respond() with the secret, failed(kind 0 = interactive), succeeded()
// with hadPrompt false meaning no password was needed.
import QtQuick
import QtQuick.Effects
import Qt.labs.folderlistmodel

import org.kde.coreaddons as KCoreAddons
import org.kde.plasma.private.keyboardindicator as KeyboardIndicator
import org.kde.plasma.private.sessions

Item {
    id: lockUi

    readonly property color accent: cfg("accent", "#33ff66")
    readonly property color secondary: cfg("secondary", "#00e5ff")
    readonly property color danger: cfg("danger", "#ff3355")
    readonly property string mono: cfg("font", "Hack")
    readonly property real unit: Math.max(12, Math.min(width / 110, height / 48))
    // kscreenlocker_userName is the account's full name ("Aaron Scruggs");
    // show the login name like the SDDM theme does, falling back to it.
    readonly property string user: kuser.loginName || kscreenlocker_userName
    // The greeter exposes no host name; reuse kwe-sddm-sync's marker.
    readonly property string host: cfg("hostname", "") || poolHost || "localhost"
    property string poolHost: ""

    property bool uiVisible: false
    property bool seenPositionChange: false
    // Authentication already succeeded without a prompt (e.g. no password).
    property bool noPassword: false
    property int failures: 0
    property date lockedAt: new Date()
    property bool powerFocused: false
    readonly property bool blockFade: passwordField.text.length > 0 || powerFocused

    KCoreAddons.KUser { id: kuser }

    function cfg(key, fallback) {
        const value = config[key];
        return value !== undefined && value !== null && value !== "" ? value : fallback;
    }

    function wake() {
        uiVisible = true;
        if (!blockFade)
            fadeTimer.restart();
    }

    function unlock() {
        if (noPassword) {
            Qt.quit();
            return;
        }
        if (graceTimer.running)
            return;
        status.show("verifying " + user + "@" + host + " ...", secondary, false);
        authenticator.respond(passwordField.text);
    }

    onUiVisibleChanged: {
        if (uiVisible) {
            Window.window.requestActivate();
            boot.replay();
            fadeTimer.restart();
        } else {
            status.show("", secondary, false);
        }
        passwordField.forceActiveFocus();
        authenticator.startAuthenticating();
    }
    onBlockFadeChanged: blockFade ? fadeTimer.stop() : fadeTimer.restart()

    Component.onCompleted: passwordField.forceActiveFocus()

    Connections {
        target: authenticator

        function onFailed(kind) {
            if (kind !== 0)
                return; // fingerprint/smartcard retries are not shown as denials
            lockUi.failures += 1;
            status.show("ACCESS DENIED :: attempt " + lockUi.failures, lockUi.danger, true);
            shake.restart();
            graceTimer.restart();
        }

        function onSucceeded() {
            if (authenticator.hadPrompt) {
                Qt.quit();
                return;
            }
            lockUi.noPassword = true;
            lockUi.wake();
            status.show("ACCESS GRANTED :: no password required — [enter] to unlock", lockUi.accent, false);
        }

        function onInfoMessageChanged() {
            if (authenticator.infoMessage)
                status.show(authenticator.infoMessage, lockUi.secondary, false);
        }

        function onErrorMessageChanged() {
            if (authenticator.errorMessage)
                status.show(authenticator.errorMessage, lockUi.danger, true);
        }

        function onPromptChanged() {
            if (authenticator.prompt && !authenticator.promptForSecret)
                status.show(authenticator.prompt, lockUi.secondary, false);
        }

        function onPromptForSecretChanged() {
            passwordField.forceActiveFocus();
        }
    }

    Connections {
        target: root

        function onClearPassword() {
            PasswordSync.password = "";
        }

        function onNotificationRepeated() {
            status.show(status.message, status.tone, true);
        }
    }

    Connections {
        target: PasswordSync

        function onPasswordChanged() {
            if (passwordField.text !== PasswordSync.password)
                passwordField.text = PasswordSync.password;
        }
    }

    SessionManagement {
        id: sessionManagement

        onAboutToSuspend: root.clearPassword()
    }

    KeyboardIndicator.KeyState {
        id: capsLock

        key: Qt.Key_CapsLock
    }

    FolderListModel {
        // "<hostname>.host": a name-only marker; nothing is read from it.
        folder: "file://" + lockUi.cfg("poolDir", "/var/lib/kwe-sddm/pool")
        nameFilters: ["*.host"]
        showDirs: false
        onCountChanged: if (count > 0) lockUi.poolHost = String(get(0, "fileBaseName"))
    }

    Timer {
        id: fadeTimer

        interval: 10000
        onTriggered: if (!lockUi.blockFade) lockUi.uiVisible = false
    }

    Timer {
        // After a denial: hold input briefly, then restart the PAM conversation.
        id: graceTimer

        interval: 3000
        onTriggered: {
            root.clearPassword();
            authenticator.startAuthenticating();
            status.show("[ .. ] awaiting credentials", lockUi.secondary, false);
        }
    }

    MouseArea {
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: lockUi.uiVisible ? Qt.ArrowCursor : Qt.BlankCursor
        onPressed: lockUi.wake()
        // The first motion event can be synthetic (pointer under the new
        // surface), so only the second one wakes the UI.
        onPositionChanged: {
            if (lockUi.seenPositionChange)
                lockUi.wake();
            lockUi.seenPositionChange = true;
        }
    }

    // ---- CRT treatment over the live wallpaper ----

    Rectangle {
        anchors.fill: parent
        color: "black"
        opacity: Number(lockUi.uiVisible ? lockUi.cfg("dim", 0.35) : lockUi.cfg("idleDim", 0.1))
        Behavior on opacity { NumberAnimation { duration: 400 } }
    }

    Canvas {
        // Vignette, painted once per resize.
        anchors.fill: parent
        onWidthChanged: requestPaint()
        onHeightChanged: requestPaint()
        onPaint: {
            const ctx = getContext("2d");
            const r = Math.max(width, height) * 0.75;
            const g = ctx.createRadialGradient(width / 2, height / 2, r * 0.35, width / 2, height / 2, r);
            g.addColorStop(0, "rgba(0,0,0,0)");
            g.addColorStop(1, "rgba(0,0,0,0.85)");
            ctx.fillStyle = g;
            ctx.fillRect(0, 0, width, height);
        }
    }

    Canvas {
        id: scanlines

        anchors.fill: parent
        visible: String(lockUi.cfg("scanlines", true)) === "true"
        onWidthChanged: requestPaint()
        onHeightChanged: requestPaint()
        onPaint: {
            const ctx = getContext("2d");
            ctx.clearRect(0, 0, width, height);
            ctx.fillStyle = "rgba(0,0,0,0.22)";
            for (let y = 0; y < height; y += 3)
                ctx.fillRect(0, y, width, 1);
        }
    }

    Rectangle {
        // Rolling refresh band.
        visible: scanlines.visible
        width: parent.width
        height: parent.height * 0.18
        gradient: Gradient {
            GradientStop { position: 0; color: "transparent" }
            GradientStop { position: 0.5; color: Qt.rgba(lockUi.accent.r, lockUi.accent.g, lockUi.accent.b, 0.05) }
            GradientStop { position: 1; color: "transparent" }
        }
        NumberAnimation on y { from: -lockUi.height * 0.2; to: lockUi.height; duration: 7000; loops: Animation.Infinite }
    }

    QtObject {
        id: clock

        property date now: new Date()
        property Timer tick: Timer { interval: 1000; running: true; repeat: true; onTriggered: clock.now = new Date() }
    }

    // ---- idle: big clock ----

    Column {
        anchors.centerIn: parent
        spacing: lockUi.unit * 0.4
        opacity: lockUi.uiVisible ? 0 : 1
        visible: opacity > 0
        Behavior on opacity { NumberAnimation { duration: 400 } }

        HackerText {
            anchors.horizontalCenter: parent.horizontalCenter
            text: Qt.formatTime(clock.now, "HH:mm")
            font.pixelSize: lockUi.unit * 7
            font.bold: true
            color: lockUi.accent
            glow: true
        }
        HackerText {
            anchors.horizontalCenter: parent.horizontalCenter
            text: Qt.formatDate(clock.now, "yyyy-MM-dd  dddd").toLowerCase()
            font.pixelSize: lockUi.unit * 1.2
            opacity: 0.8
        }
        HackerText {
            anchors.horizontalCenter: parent.horizontalCenter
            text: "[ session locked :: press any key ]"
            SequentialAnimation on opacity {
                loops: Animation.Infinite
                running: !lockUi.uiVisible
                NumberAnimation { to: 0.15; duration: 900; easing.type: Easing.InOutSine }
                NumberAnimation { to: 0.6; duration: 900; easing.type: Easing.InOutSine }
            }
        }
    }

    // ---- awake: top bar, terminal, power ----

    Item {
        id: awake

        anchors.fill: parent
        opacity: lockUi.uiVisible ? 1 : 0
        visible: opacity > 0
        Behavior on opacity { NumberAnimation { duration: 250 } }

        Item {
            anchors { left: parent.left; right: parent.right; top: parent.top; margins: lockUi.unit * 1.5 }
            height: topClock.height

            Column {
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                spacing: lockUi.unit * 0.2

                HackerText {
                    text: "kwe://" + lockUi.host
                    color: lockUi.accent
                    font.pixelSize: lockUi.unit * 1.1
                    font.bold: true
                }
                HackerText {
                    text: "locked :: " + lockUi.user + " :: since " + Qt.formatTime(lockUi.lockedAt, "HH:mm")
                    opacity: 0.7
                }
            }

            Column {
                id: topClock

                anchors.right: parent.right
                spacing: lockUi.unit * 0.1

                HackerText {
                    anchors.right: parent.right
                    text: Qt.formatTime(clock.now, "HH:mm:ss")
                    font.pixelSize: lockUi.unit * 3.2
                    font.bold: true
                    color: lockUi.accent
                    glow: true
                }
                HackerText {
                    anchors.right: parent.right
                    text: Qt.formatDate(clock.now, "yyyy-MM-dd  dddd").toLowerCase()
                    opacity: 0.75
                }
            }
        }

        Rectangle {
            id: terminal

            property real baseX: (parent.width - width) / 2
            x: baseX
            anchors.verticalCenter: parent.verticalCenter
            width: Math.min(lockUi.unit * 46, parent.width - lockUi.unit * 4)
            height: titleBar.height + termColumn.implicitHeight + lockUi.unit * 2.4
            color: Qt.rgba(0.01, 0.03, 0.02, 0.78)
            border.color: Qt.rgba(lockUi.accent.r, lockUi.accent.g, lockUi.accent.b, 0.55)
            border.width: 1

            SequentialAnimation {
                id: shake

                NumberAnimation { target: terminal; property: "x"; to: terminal.baseX - 14; duration: 40 }
                NumberAnimation { target: terminal; property: "x"; to: terminal.baseX + 12; duration: 50 }
                NumberAnimation { target: terminal; property: "x"; to: terminal.baseX - 8; duration: 50 }
                NumberAnimation { target: terminal; property: "x"; to: terminal.baseX + 4; duration: 50 }
                NumberAnimation { target: terminal; property: "x"; to: terminal.baseX; duration: 40 }
            }

            Rectangle {
                id: titleBar

                anchors { left: parent.left; right: parent.right; top: parent.top }
                height: lockUi.unit * 1.7
                color: Qt.rgba(lockUi.accent.r, lockUi.accent.g, lockUi.accent.b, 0.14)

                HackerText {
                    anchors.verticalCenter: parent.verticalCenter
                    anchors.left: parent.left
                    anchors.leftMargin: lockUi.unit * 0.8
                    text: "── secure-shell :: unlock ── " + lockUi.host
                    opacity: 0.85
                }
                HackerText {
                    anchors.verticalCenter: parent.verticalCenter
                    anchors.right: parent.right
                    anchors.rightMargin: lockUi.unit * 0.8
                    text: "■ □ ×"
                    opacity: 0.5
                }
            }

            Column {
                id: termColumn

                anchors { left: parent.left; right: parent.right; top: titleBar.bottom; margins: lockUi.unit * 1.2 }
                spacing: lockUi.unit * 0.45

                BootLog {
                    id: boot

                    width: parent.width
                    lines: [
                        "[ OK ] session locked :: " + lockUi.user + "@" + lockUi.host,
                        "[ OK ] wallpaper engine :: live frame link",
                        "[ OK ] entropy pool seeded :: " + Math.floor(Math.random() * 0xffffffff).toString(16),
                        "[ .. ] awaiting credentials"
                    ]
                }

                Item { width: 1; height: lockUi.unit * 0.4 }

                PromptRow {
                    label: "login"
                    width: parent.width
                    field: HackerText {
                        text: lockUi.user
                        color: lockUi.accent
                        font.pixelSize: lockUi.unit * 1.05
                    }
                }

                PromptRow {
                    label: lockUi.noPassword ? "unlock" : "password"
                    width: parent.width
                    field: LineInput {
                        id: passwordField

                        // A non-secret PAM prompt (rare) is answered in clear text.
                        echoMode: authenticator.prompt && !authenticator.promptForSecret ? TextInput.Normal : TextInput.Password
                        passwordCharacter: "*"
                        readOnly: graceTimer.running || lockUi.noPassword
                        Accessible.name: qsTr("Password")
                        onTextEdited: PasswordSync.password = text
                        Keys.onPressed: (event) => {
                            lockUi.wake();
                            event.accepted = false;
                        }
                        Keys.onReturnPressed: lockUi.unlock()
                        Keys.onEnterPressed: lockUi.unlock()
                        Keys.onEscapePressed: {
                            // kscreenlocker blanks the screen on Escape; drop the prompt too.
                            if (lockUi.uiVisible) {
                                lockUi.uiVisible = false;
                                root.clearPassword();
                            }
                        }
                    }
                }

                Item { width: 1; height: lockUi.unit * 0.3 }

                StatusLine {
                    id: status

                    width: parent.width
                }

                HackerText {
                    visible: capsLock.locked
                    text: "!! CAPS LOCK ENGAGED"
                    color: lockUi.danger
                    font.bold: true
                }

                HackerText {
                    width: parent.width
                    wrapMode: Text.WordWrap
                    text: "[enter] unlock  [tab] power  [esc] blank"
                    opacity: 0.45
                    font.pixelSize: lockUi.unit * 0.75
                }
            }
        }

        Row {
            // Reached with Tab from the password field (default tab chain).
            anchors { right: parent.right; bottom: parent.bottom; margins: lockUi.unit * 1.5 }
            spacing: lockUi.unit * 1.2

            PowerButton { label: "suspend"; visible: sessionManagement.canSuspend; onActivated: sessionManagement.suspend() }
            PowerButton { label: "hibernate"; visible: sessionManagement.canHibernate; onActivated: sessionManagement.hibernate() }
            PowerButton { label: "switch-user"; visible: sessionManagement.canSwitchUser; onActivated: sessionManagement.switchUser() }
        }

        HackerText {
            anchors { left: parent.left; bottom: parent.bottom; margins: lockUi.unit * 1.5 }
            text: "▶ live :: kde wallpaper engine"
            opacity: 0.55
        }
    }

    Loader {
        // Plasma's volume/brightness OSD (the greeter finds it by objectName
        // "onScreenDisplay"); optional, so a missing file only costs the OSD.
        z: 2
        active: root.viewVisible
        source: "file:///usr/share/plasma/shells/org.kde.plasma.desktop/contents/lockscreen/LockOsd.qml"
        anchors { horizontalCenter: parent.horizontalCenter; bottom: parent.bottom; bottomMargin: lockUi.unit }
    }

    // ---- components (shared look with sddm/kwe-terminal) ----

    component HackerText: Text {
        id: hackerText

        property bool glow: false

        color: lockUi.secondary
        font.family: lockUi.mono
        font.pixelSize: lockUi.unit * 0.95
        renderType: Text.NativeRendering

        // The glow is a sibling effect behind the text, not the text's own
        // layer: if the shader cannot run (software rendering), only the
        // glow is lost and the text stays readable.
        Text {
            id: glowSource

            visible: false
            text: hackerText.text
            color: hackerText.color
            font: hackerText.font
        }
        MultiEffect {
            z: -1
            anchors.fill: parent
            visible: hackerText.glow
            source: glowSource
            shadowEnabled: true
            shadowColor: lockUi.accent
            shadowBlur: 0.8
            shadowHorizontalOffset: 0
            shadowVerticalOffset: 0
        }
    }

    component LineInput: TextInput {
        color: lockUi.accent
        font.family: lockUi.mono
        font.pixelSize: lockUi.unit * 1.05
        selectionColor: lockUi.accent
        selectedTextColor: "black"
        clip: true
        activeFocusOnTab: true
        cursorDelegate: Rectangle {
            width: lockUi.unit * 0.6
            color: lockUi.accent
            visible: parent.activeFocus
            SequentialAnimation on opacity {
                loops: Animation.Infinite
                NumberAnimation { to: 0; duration: 0 }
                PauseAnimation { duration: 530 }
                NumberAnimation { to: 1; duration: 0 }
                PauseAnimation { duration: 530 }
            }
        }
    }

    component PromptRow: Row {
        id: row

        property string label
        property Item field

        spacing: lockUi.unit * 0.5
        height: field ? field.height : lockUi.unit

        HackerText {
            id: labelText

            width: lockUi.unit * 6.5
            text: row.label + ":"
            color: row.field && row.field.activeFocus ? lockUi.accent : lockUi.secondary
            font.bold: row.field && row.field.activeFocus
        }
        Item {
            width: row.width - labelText.width - row.spacing
            height: row.field ? row.field.height : 0
            Component.onCompleted: if (row.field) {
                row.field.parent = this;
                row.field.width = Qt.binding(() => width);
            }
            Rectangle {
                anchors { left: parent.left; right: parent.right; bottom: parent.bottom; bottomMargin: -lockUi.unit * 0.15 }
                height: 1
                color: lockUi.accent
                opacity: row.field && row.field.activeFocus ? 0.6 : 0.15
            }
        }
    }

    component BootLog: Column {
        id: log

        property var lines: []
        property int shownLines: 0
        property int shownChars: 0

        function replay() {
            shownLines = 0;
            shownChars = 0;
        }

        spacing: lockUi.unit * 0.25

        Repeater {
            model: log.lines.length
            delegate: HackerText {
                required property int index
                width: log.width
                elide: Text.ElideRight
                visible: index <= log.shownLines
                text: {
                    const full = log.lines[index];
                    return index < log.shownLines ? full : full.slice(0, log.shownChars);
                }
                color: log.lines[index].startsWith("[ OK ]") ? lockUi.accent : lockUi.secondary
                opacity: 0.9
            }
        }

        Timer {
            // Typewriter, ~1 s; decoration only, the password field already has focus.
            interval: 10
            repeat: true
            running: log.shownLines < log.lines.length && lockUi.uiVisible
            onTriggered: {
                const line = log.lines[log.shownLines] || "";
                log.shownChars += 3;
                if (log.shownChars >= line.length) {
                    log.shownLines += 1;
                    log.shownChars = 0;
                }
            }
        }
    }

    component StatusLine: Item {
        id: line

        property string message: ""
        property color tone: lockUi.secondary
        property bool glitch: false

        function show(text, color, glitchy) {
            message = text;
            tone = color;
            glitch = glitchy;
            if (glitchy)
                glitchAnim.restart();
        }

        height: main.height
        Accessible.role: Accessible.StaticText
        Accessible.name: message

        // RGB-split ghosts for the failure glitch.
        HackerText { x: 3; text: main.text; color: "#ff0044"; opacity: ghost.value; font.bold: true }
        HackerText { x: -3; text: main.text; color: "#00e5ff"; opacity: ghost.value; font.bold: true }
        HackerText {
            id: main

            width: line.width
            wrapMode: Text.WordWrap
            text: line.message.length > 0 ? "> " + line.message : ""
            color: line.tone
            font.bold: true
            glow: line.glitch
        }

        QtObject { id: ghost; property real value: 0 }
        SequentialAnimation {
            id: glitchAnim

            loops: 3
            NumberAnimation { target: ghost; property: "value"; to: 0.8; duration: 40 }
            NumberAnimation { target: ghost; property: "value"; to: 0; duration: 90 }
            PauseAnimation { duration: 60 }
        }
    }

    component PowerButton: HackerText {
        id: button

        property string label
        signal activated

        text: "[ " + label + " ]"
        color: activeFocus || hover.containsMouse ? lockUi.danger : lockUi.secondary
        opacity: activeFocus || hover.containsMouse ? 1 : 0.7
        activeFocusOnTab: true
        Accessible.role: Accessible.Button
        Accessible.name: label
        Accessible.onPressAction: activated()
        Keys.onReturnPressed: activated()
        Keys.onEnterPressed: activated()
        Keys.onSpacePressed: activated()
        Keys.onEscapePressed: passwordField.forceActiveFocus()
        onActiveFocusChanged: lockUi.powerFocused = activeFocus

        MouseArea {
            id: hover

            anchors.fill: parent
            hoverEnabled: true
            cursorShape: Qt.PointingHandCursor
            onClicked: button.activated()
        }
    }
}
