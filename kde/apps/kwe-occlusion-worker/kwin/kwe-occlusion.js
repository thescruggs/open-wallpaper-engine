// SPDX-License-Identifier: GPL-3.0-or-later
// KDE Wallpaper Engine occlusion detector (F3). Loaded into KWin by
// kwe-occlusion-worker through org.kde.kwin.Scripting; reports, per output,
// whether a normal window on the current desktop/activity is fullscreen or
// covers the output's whole maximize area. One D-Bus call per transition —
// the report string is compared before sending, so window drags and
// per-frame geometry updates never spam the bus. The bridge debounces and
// relays to kwe-daemon as `occlusion.report`; the daemon applies the
// "every output covered" policy.
"use strict";

const SERVICE = "org.kde.kwe.Occlusion1";
const PATH = "/org/kde/kwe/Occlusion";
const INTERFACE = "org.kde.kwe.Occlusion1";

let lastReport = "";

function rectCovers(outer, inner) {
    // One pixel of slack: fractional frame geometry on scaled outputs.
    return outer.x <= inner.x + 1 && outer.y <= inner.y + 1 &&
        outer.x + outer.width >= inner.x + inner.width - 1 &&
        outer.y + outer.height >= inner.y + inner.height - 1;
}

function sameDesktop(a, b) {
    if (a === b) return true;
    return !!(a && b && a.id !== undefined && a.id === b.id);
}

function onCurrentDesktop(window) {
    if (window.onAllDesktops) return true;
    const current = workspace.currentDesktop;
    const desktops = window.desktops || [];
    for (let i = 0; i < desktops.length; ++i) {
        if (sameDesktop(desktops[i], current)) return true;
    }
    return false;
}

function onCurrentActivity(window) {
    const activities = window.activities || [];
    if (activities.length === 0) return true;
    return activities.indexOf(workspace.currentActivity) >= 0;
}

function maximizeArea(window) {
    try {
        return workspace.clientArea(KWin.MaximizeArea, window);
    } catch (error) {
        return window.output ? window.output.geometry : null;
    }
}

function windowCovers(window) {
    if (!window || window.deleted || window.minimized) return false;
    if (!window.normalWindow) return false;
    if (!onCurrentDesktop(window) || !onCurrentActivity(window)) return false;
    if (window.fullScreen) return true;
    const area = maximizeArea(window);
    if (!area) return false;
    return rectCovers(window.frameGeometry, area);
}

function evaluate() {
    const outputs = [];
    const screens = workspace.screens || [];
    for (let i = 0; i < screens.length; ++i) {
        if (screens[i] && screens[i].name) outputs.push(screens[i].name);
    }
    const covered = [];
    const windows = workspace.windowList();
    for (let i = 0; i < windows.length; ++i) {
        const window = windows[i];
        if (!windowCovers(window)) continue;
        const name = window.output ? window.output.name : null;
        if (name && covered.indexOf(name) < 0) covered.push(name);
    }
    outputs.sort();
    covered.sort();
    const report = JSON.stringify({ outputs: outputs, covered: covered });
    if (report === lastReport) return;
    lastReport = report;
    callDBus(SERVICE, PATH, INTERFACE, "Report", report);
}

const WINDOW_SIGNALS = [
    "frameGeometryChanged", "minimizedChanged", "fullScreenChanged",
    "desktopsChanged", "outputChanged", "activitiesChanged", "closed",
];

function track(window) {
    if (!window) return;
    for (let i = 0; i < WINDOW_SIGNALS.length; ++i) {
        const signal = window[WINDOW_SIGNALS[i]];
        if (signal && typeof signal.connect === "function") signal.connect(evaluate);
    }
}

workspace.windowAdded.connect(function (window) {
    track(window);
    evaluate();
});
workspace.windowRemoved.connect(evaluate);
workspace.currentDesktopChanged.connect(evaluate);
if (workspace.currentActivityChanged) workspace.currentActivityChanged.connect(evaluate);
if (workspace.screensChanged) workspace.screensChanged.connect(evaluate);

const initial = workspace.windowList();
for (let i = 0; i < initial.length; ++i) track(initial[i]);
evaluate();
