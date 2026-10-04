// SPDX-License-Identifier: GPL-3.0-or-later
// KWE Terminal: SDDM greeter that plays a random clip from the KDE
// Wallpaper Engine login pool (filled by kwe-sddm-sync) behind a
// terminal/CRT login. The greeter only lists a directory and plays H.264;
// no wallpaper content is parsed or rendered here.
import QtQuick
import QtQuick.Effects
import QtMultimedia
import QtQml.Models
import Qt.labs.folderlistmodel

Rectangle {
    id: root

    width: 1920
    height: 1080
    color: cfg("fallbackColor", "#05070a")

    // SDDM sets primaryScreen per view; the login lives on the primary only.
    readonly property bool isPrimary: typeof primaryScreen === "undefined" || primaryScreen
    readonly property color accent: cfg("accent", "#33ff66")
    readonly property color secondary: cfg("secondary", "#00e5ff")
    readonly property color danger: cfg("danger", "#ff3355")
    readonly property string mono: cfg("font", "Hack")
    readonly property real unit: Math.max(12, Math.min(width / 110, height / 48))
    // The daemon supplies sddm.hostName; preview (--test-mode) has none, so
    // fall back to the marker kwe-sddm-sync writes, then theme.conf.
    readonly property string host: sddm.hostName || poolHost || cfg("hostname", "localhost")
    property string poolHost: ""

    // Picked pool entry, parsed from "<kind>-<id>--<Title-Slug>.mp4".
    property url clipUrl: ""
    property url posterUrl: ""
    property string clipKind: ""
    property string clipId: ""
    property string clipTitle: ""

    property int userIndex: Math.max(0, userModel.lastIndex)
    property int sessionIndex: Math.max(0, sessionModel.lastIndex)
    // Filled as the Instantiators create rows; objectAt() alone is not a
    // notifiable dependency, so bindings read these instead.
    property var userNames: []
    property var sessionNames: []
    property int failures: 0
    property bool busy: false

    function cfg(key, fallback) {
        const value = config.stringValue ? config.stringValue(key) : config[key];
        return value !== undefined && value !== null && value !== "" ? value : fallback;
    }

    function pickClip() {
        if (pool.count === 0)
            return;
        const index = Math.floor(Math.random() * pool.count);
        const base = String(pool.get(index, "fileBaseName"));
        clipUrl = pool.get(index, "fileUrl");
        posterUrl = String(clipUrl).replace(/\.mp4$/, ".jpg");
        const split = base.indexOf("--");
        const head = split >= 0 ? base.slice(0, split) : base;
        const dash = head.indexOf("-");
        clipKind = dash >= 0 ? head.slice(0, dash) : "wallpaper";
        clipId = dash >= 0 ? head.slice(dash + 1) : "";
        clipTitle = split >= 0 ? base.slice(split + 2).replace(/-/g, " ") : base;
    }

    function userName(i) {
        return userNames[i] || "";
    }

    function sessionName(i) {
        return sessionNames[i] || "default";
    }

    function cycleUser(step) {
        if (userModel.count === 0)
            return;
        userIndex = (userIndex + step + userModel.count) % userModel.count;
        loginField.text = userName(userIndex);
        passwordField.text = "";
    }

    function cycleSession(step) {
        if (sessionModel.count === 0)
            return;
        sessionIndex = (sessionIndex + step + sessionModel.count) % sessionModel.count;
    }

    function authenticate() {
        if (busy || loginField.text.length === 0)
            return;
        busy = true;
        status.show("authenticating " + loginField.text + "@" + host + " ...", secondary, false);
        sddm.login(loginField.text, passwordField.text, sessionIndex);
    }

    FolderListModel {
        id: pool

        folder: "file://" + root.cfg("poolDir", "/var/lib/kwe-sddm/pool")
        nameFilters: ["*.mp4"]
        showDirs: false
        showHidden: false
        // Ready can arrive before the rows do; pick on the first non-empty count.
        onCountChanged: if (count > 0 && root.clipUrl == "") root.pickClip()
    }

    FolderListModel {
        // "<hostname>.host": a name-only marker; nothing is read from it.
        folder: pool.folder
        nameFilters: ["*.host"]
        showDirs: false
        onCountChanged: if (count > 0) root.poolHost = String(get(0, "fileBaseName"))
    }

    Instantiator {
        id: users

        model: userModel
        delegate: QtObject {
            required property string name
        }
        onObjectAdded: (index, object) => {
            const names = root.userNames.slice();
            names[index] = object.name;
            root.userNames = names;
        }
    }

    Instantiator {
        id: sessions

        model: sessionModel
        delegate: QtObject {
            required property string name
        }
        onObjectAdded: (index, object) => {
            const names = root.sessionNames.slice();
            names[index] = object.name;
            root.sessionNames = names;
        }
    }

    Connections {
        target: sddm

        function onLoginSucceeded() {
            status.show("ACCESS GRANTED :: spawning session", root.accent, false);
        }

        function onLoginFailed() {
            root.busy = false;
            root.failures += 1;
            passwordField.text = "";
            passwordField.forceActiveFocus();
            status.show("ACCESS DENIED :: attempt " + root.failures, root.danger, true);
            shake.restart();
        }
    }

    // ---- background: poster, then the looping clip over it ----

    Image {
        anchors.fill: parent
        source: root.posterUrl
        fillMode: Image.PreserveAspectCrop
        asynchronous: true
        visible: status === Image.Ready
    }

    Video {
        id: clip

        anchors.fill: parent
        source: root.clipUrl
        fillMode: VideoOutput.PreserveAspectCrop
        loops: MediaPlayer.Infinite
        muted: true
        autoPlay: true
        opacity: playbackState === MediaPlayer.PlayingState ? 1 : 0
        Behavior on opacity { NumberAnimation { duration: 600 } }
    }

    // Fallback when the pool is empty: a slowly scrolling grid. Painted
    // once and moved, since repainting a full-screen Canvas every frame
    // starves the UI thread (the boot log's typewriter timer stalls).
    Item {
        anchors.fill: parent
        clip: true
        visible: root.clipUrl == ""

        Canvas {
            id: grid

            width: parent.width
            height: parent.height + 48
            onWidthChanged: requestPaint()
            onHeightChanged: requestPaint()
            NumberAnimation on y { from: -48; to: 0; duration: 4000; loops: Animation.Infinite; running: grid.parent.visible }
            onPaint: {
                const ctx = getContext("2d");
                ctx.clearRect(0, 0, width, height);
                ctx.strokeStyle = Qt.rgba(root.accent.r, root.accent.g, root.accent.b, 0.08);
                ctx.lineWidth = 1;
                ctx.beginPath();
                for (let x = 0; x < width; x += 48) {
                    ctx.moveTo(x + 0.5, 0); ctx.lineTo(x + 0.5, height);
                }
                for (let y = 0; y < height; y += 48) {
                    ctx.moveTo(0, y + 0.5); ctx.lineTo(width, y + 0.5);
                }
                ctx.stroke();
            }
        }
    }

    // ---- CRT treatment ----

    Rectangle {
        anchors.fill: parent
        color: "black"
        opacity: Number(root.cfg("dim", "0.35"))
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
        visible: root.cfg("scanlines", "true") === "true"
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
            GradientStop { position: 0.5; color: Qt.rgba(root.accent.r, root.accent.g, root.accent.b, 0.05) }
            GradientStop { position: 1; color: "transparent" }
        }
        NumberAnimation on y { from: -root.height * 0.2; to: root.height; duration: 7000; loops: Animation.Infinite }
    }

    // ---- top bar ----

    Item {
        id: topBar

        anchors { left: parent.left; right: parent.right; top: parent.top; margins: root.unit * 1.5 }
        height: clock.height

        Column {
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            spacing: root.unit * 0.2

            HackerText {
                text: "kwe://" + root.host
                color: root.accent
                font.pixelSize: root.unit * 1.1
                font.bold: true
            }
            HackerText {
                visible: root.isPrimary
                text: "tty1 :: " + root.sessionName(root.sessionIndex).toLowerCase()
                      + (keyboard.layouts.length > 1 ? " :: kbd " + keyboard.layouts[keyboard.currentLayout].shortName : "")
                opacity: 0.7
            }
        }

        Column {
            id: clock

            anchors.right: parent.right
            spacing: root.unit * 0.1
            property date now: new Date()
            Timer { interval: 1000; running: true; repeat: true; onTriggered: clock.now = new Date() }

            HackerText {
                anchors.right: parent.right
                text: Qt.formatTime(clock.now, "HH:mm:ss")
                font.pixelSize: root.unit * 3.2
                font.bold: true
                color: root.accent
                glow: true
            }
            HackerText {
                anchors.right: parent.right
                text: Qt.formatDate(clock.now, "yyyy-MM-dd  dddd").toLowerCase()
                opacity: 0.75
            }
        }
    }

    // ---- terminal ----

    Rectangle {
        id: terminal

        visible: root.isPrimary
        property real baseX: (root.width - width) / 2
        x: baseX
        anchors.verticalCenter: parent.verticalCenter
        width: Math.min(root.unit * 46, root.width - root.unit * 4)
        height: titleBar.height + termColumn.implicitHeight + root.unit * 2.4
        color: Qt.rgba(0.01, 0.03, 0.02, 0.78)
        border.color: Qt.rgba(root.accent.r, root.accent.g, root.accent.b, 0.55)
        border.width: 1

        SequentialAnimation {
            id: shake

            NumberAnimation { target: terminal; property: "x"; to: terminal.baseX - 14; duration: 40 }
            NumberAnimation { target: terminal; property: "x"; to: terminal.baseX + 12; duration: 50 }
            NumberAnimation { target: terminal; property: "x"; to: terminal.baseX - 8; duration: 50 }
            NumberAnimation { target: terminal; property: "x"; to: terminal.baseX + 4; duration: 50 }
            NumberAnimation { target: terminal; property: "x"; to: terminal.baseX; duration: 40 }
        }

        // Title bar.
        Rectangle {
            id: titleBar

            anchors { left: parent.left; right: parent.right; top: parent.top }
            height: root.unit * 1.7
            color: Qt.rgba(root.accent.r, root.accent.g, root.accent.b, 0.14)

            HackerText {
                anchors.verticalCenter: parent.verticalCenter
                anchors.left: parent.left
                anchors.leftMargin: root.unit * 0.8
                text: "── secure-shell :: auth ── " + root.host
                opacity: 0.85
            }
            HackerText {
                anchors.verticalCenter: parent.verticalCenter
                anchors.right: parent.right
                anchors.rightMargin: root.unit * 0.8
                text: "■ □ ×"
                opacity: 0.5
            }
        }

        Column {
            id: termColumn

            anchors { left: parent.left; right: parent.right; top: titleBar.bottom; margins: root.unit * 1.2 }
            spacing: root.unit * 0.45

            BootLog {
                id: boot

                width: parent.width
                lines: [
                    "[ OK ] link established :: " + root.host,
                    "[ OK ] wallpaper engine :: " + (root.clipUrl != ""
                        ? (root.cfg("showWallpaperTitle", "true") === "true" ? root.clipTitle : "clip")
                          + "  <" + root.clipKind + (root.clipId ? " #" + root.clipId : "") + ">"
                        : "login pool empty — run kwe-sddm-sync"),
                    "[ OK ] entropy pool seeded :: " + Math.floor(Math.random() * 0xffffffff).toString(16),
                    "[ .. ] awaiting credentials"
                ]
                onFinished: loginField.forceActiveFocus()
            }

            Item { width: 1; height: root.unit * 0.4 }

            PromptRow {
                label: "login"
                width: parent.width
                field: LineInput {
                    id: loginField

                    text: root.userName(root.userIndex) || userModel.lastUser
                    Accessible.name: qsTr("User name")
                    KeyNavigation.tab: passwordField
                    Keys.onUpPressed: root.cycleUser(-1)
                    Keys.onDownPressed: root.cycleUser(1)
                    Keys.onReturnPressed: passwordField.forceActiveFocus()
                    Keys.onEnterPressed: passwordField.forceActiveFocus()
                }
            }

            PromptRow {
                label: "password"
                width: parent.width
                field: LineInput {
                    id: passwordField

                    echoMode: TextInput.Password
                    passwordCharacter: "*"
                    Accessible.name: qsTr("Password")
                    KeyNavigation.tab: sessionPicker
                    KeyNavigation.backtab: loginField
                    Keys.onReturnPressed: root.authenticate()
                    Keys.onEnterPressed: root.authenticate()
                }
            }

            PromptRow {
                label: "session"
                width: parent.width
                field: FocusScope {
                    id: sessionPicker

                    height: sessionText.height
                    activeFocusOnTab: true
                    Accessible.role: Accessible.ComboBox
                    Accessible.name: qsTr("Session: %1").arg(root.sessionName(root.sessionIndex))
                    KeyNavigation.tab: loginField
                    KeyNavigation.backtab: passwordField
                    Keys.onLeftPressed: root.cycleSession(-1)
                    Keys.onRightPressed: root.cycleSession(1)
                    Keys.onSpacePressed: root.cycleSession(1)
                    Keys.onReturnPressed: root.authenticate()
                    Keys.onEnterPressed: root.authenticate()

                    HackerText {
                        id: sessionText

                        text: "< " + root.sessionName(root.sessionIndex) + " >"
                        color: sessionPicker.activeFocus ? root.accent : root.secondary
                    }
                    MouseArea {
                        anchors.fill: sessionText
                        acceptedButtons: Qt.LeftButton | Qt.RightButton
                        cursorShape: Qt.PointingHandCursor
                        onClicked: (mouse) => root.cycleSession(mouse.button === Qt.RightButton ? -1 : 1)
                    }
                }
            }

            Item { width: 1; height: root.unit * 0.3 }

            StatusLine {
                id: status

                width: parent.width
            }

            HackerText {
                visible: keyboard.capsLock
                text: "!! CAPS LOCK ENGAGED"
                color: root.danger
                font.bold: true
            }

            HackerText {
                width: parent.width
                wrapMode: Text.WordWrap
                text: "[enter] auth  [tab] next  [↑↓] user  [←→] session  [f10] power"
                opacity: 0.45
                font.pixelSize: root.unit * 0.75
            }
        }
    }

    // ---- power ----

    Row {
        id: power

        visible: root.isPrimary
        anchors { right: parent.right; bottom: parent.bottom; margins: root.unit * 1.5 }
        spacing: root.unit * 1.2

        PowerButton { label: "suspend"; visible: sddm.canSuspend; onActivated: sddm.suspend() }
        PowerButton { label: "hibernate"; visible: sddm.canHibernate; onActivated: sddm.hibernate() }
        PowerButton { label: "reboot"; visible: sddm.canReboot; onActivated: sddm.reboot() }
        PowerButton { id: powerOff; label: "poweroff"; visible: sddm.canPowerOff; onActivated: sddm.powerOff() }
    }

    HackerText {
        anchors { left: parent.left; bottom: parent.bottom; margins: root.unit * 1.5 }
        visible: root.clipUrl != "" && root.cfg("showWallpaperTitle", "true") === "true"
        text: "▶ " + root.clipTitle.toLowerCase()
        opacity: 0.55
    }

    Shortcut {
        sequence: "F10"
        onActivated: powerOff.forceActiveFocus()
    }

    Component.onCompleted: if (root.isPrimary) loginField.forceActiveFocus()

    // ---- components ----

    component HackerText: Text {
        property bool glow: false

        color: root.secondary
        font.family: root.mono
        font.pixelSize: root.unit * 0.95
        renderType: Text.NativeRendering
        layer.enabled: glow
        layer.effect: MultiEffect {
            shadowEnabled: true
            shadowColor: root.accent
            shadowBlur: 0.8
            shadowHorizontalOffset: 0
            shadowVerticalOffset: 0
        }
    }

    component LineInput: TextInput {
        color: root.accent
        font.family: root.mono
        font.pixelSize: root.unit * 1.05
        selectionColor: root.accent
        selectedTextColor: "black"
        clip: true
        activeFocusOnTab: true
        cursorDelegate: Rectangle {
            width: root.unit * 0.6
            color: root.accent
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

        spacing: root.unit * 0.5
        height: field ? field.height : root.unit

        HackerText {
            id: labelText

            width: root.unit * 6.5
            text: row.label + ":"
            color: row.field && row.field.activeFocus ? root.accent : root.secondary
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
                anchors { left: parent.left; right: parent.right; bottom: parent.bottom; bottomMargin: -root.unit * 0.15 }
                height: 1
                color: root.accent
                opacity: row.field && row.field.activeFocus ? 0.6 : 0.15
            }
        }
    }

    component BootLog: Column {
        id: log

        property var lines: []
        property int shownLines: 0
        property int shownChars: 0
        signal finished

        spacing: root.unit * 0.25

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
                color: log.lines[index].startsWith("[ OK ]") ? root.accent : root.secondary
                opacity: 0.9
            }
        }

        Timer {
            // Typewriter: ~2 s for the whole log, then focus moves to login.
            interval: 14
            repeat: true
            running: log.shownLines < log.lines.length
            onTriggered: {
                const line = log.lines[log.shownLines] || "";
                log.shownChars += 2;
                if (log.shownChars >= line.length) {
                    log.shownLines += 1;
                    log.shownChars = 0;
                    if (log.shownLines >= log.lines.length)
                        log.finished();
                }
            }
        }
    }

    component StatusLine: Item {
        id: line

        property string message: ""
        property color tone: root.secondary
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
        color: activeFocus || hover.containsMouse ? root.danger : root.secondary
        opacity: activeFocus || hover.containsMouse ? 1 : 0.7
        activeFocusOnTab: true
        Accessible.role: Accessible.Button
        Accessible.name: label
        Accessible.onPressAction: activated()
        Keys.onReturnPressed: activated()
        Keys.onEnterPressed: activated()
        Keys.onSpacePressed: activated()

        MouseArea {
            id: hover

            anchors.fill: parent
            hoverEnabled: true
            cursorShape: Qt.PointingHandCursor
            onClicked: button.activated()
        }
    }
}
