// SPDX-License-Identifier: GPL-3.0-or-later
// kwe-occlusion-worker (F3): a daemon-spawned helper that owns the
// org.kde.kwe.Occlusion1 session-bus name, loads the packaged KWin script
// (kwe-occlusion.js) through org.kde.kwin.Scripting, receives its per-output
// "covered" reports, and relays them to kwe-daemon as `occlusion.report`.
// It runs only while the pause-when-covered setting is on; the daemon
// terminates it (SIGTERM, then SIGKILL) when the policy is switched off and
// PR_SET_PDEATHSIG covers a crashed daemon. On exit the script is unloaded
// so nothing lingers inside KWin. A KWin restart is followed through the
// service watcher: the script reloads, and while KWin is gone the desktop
// counts as uncovered.
#include "occlusionbridge.h"

#include <QCommandLineParser>
#include <QCoreApplication>
#include <QDBusConnection>
#include <QDBusInterface>
#include <QDBusReply>
#include <QDBusServiceWatcher>
#include <QFileInfo>
#include <QSocketNotifier>
#include <QStandardPaths>

#include <csignal>
#include <cstdio>
#include <sys/socket.h>
#include <unistd.h>

namespace {
constexpr auto ServiceName = "org.kde.kwe.Occlusion1";
constexpr auto ObjectPath = "/org/kde/kwe/Occlusion";
constexpr auto KWinService = "org.kde.KWin";
constexpr auto KWinScriptingPath = "/Scripting";
constexpr auto KWinScriptingInterface = "org.kde.kwin.Scripting";
constexpr auto PluginName = "kwe-occlusion";

int signalPipe[2] = {-1, -1};

void onSignal(int) {
    const char byte = 1;
    (void)::write(signalPipe[1], &byte, 1);
}

class KWinScript {
public:
    explicit KWinScript(QString path) : m_path(std::move(path)) {}

    bool load() {
        QDBusInterface scripting{QString::fromLatin1(KWinService),
                                 QString::fromLatin1(KWinScriptingPath),
                                 QString::fromLatin1(KWinScriptingInterface)};
        if (!scripting.isValid()) {
            fprintf(stderr, "event=occlusion.kwin_unavailable detail=%s\n",
                    qPrintable(scripting.lastError().message()));
            return false;
        }
        const QDBusReply<bool> loaded = scripting.call(QStringLiteral("isScriptLoaded"),
                                                       QString::fromLatin1(PluginName));
        if (loaded.isValid() && loaded.value())
            scripting.call(QStringLiteral("unloadScript"), QString::fromLatin1(PluginName));
        const QDBusReply<int> id =
            scripting.call(QStringLiteral("loadScript"), m_path, QString::fromLatin1(PluginName));
        if (!id.isValid() || id.value() < 0) {
            fprintf(stderr, "event=occlusion.kwin_load_failed path=%s detail=%s\n",
                    qPrintable(m_path),
                    id.isValid() ? "loadScript returned -1" : qPrintable(id.error().message()));
            return false;
        }
        // Runs every loaded-but-idle script (ours); already-running ones are
        // untouched.
        scripting.call(QStringLiteral("start"));
        m_loaded = true;
        fprintf(stderr, "event=occlusion.kwin_script_loaded id=%d\n", id.value());
        return true;
    }

    void unload() {
        if (!m_loaded)
            return;
        m_loaded = false;
        QDBusInterface scripting{QString::fromLatin1(KWinService),
                                 QString::fromLatin1(KWinScriptingPath),
                                 QString::fromLatin1(KWinScriptingInterface)};
        if (scripting.isValid())
            scripting.call(QStringLiteral("unloadScript"), QString::fromLatin1(PluginName));
    }

private:
    QString m_path;
    bool m_loaded = false;
};

QString defaultScriptPath() {
    // Dev override beside the binary (build tree), then the installed data
    // location (/usr/share/kde-wallpaper-engine/kwin/kwe-occlusion.js).
    const QString beside = QCoreApplication::applicationDirPath() + QStringLiteral("/kwe-occlusion.js");
    if (QFileInfo::exists(beside))
        return beside;
    return QStandardPaths::locate(QStandardPaths::GenericDataLocation,
                                  QStringLiteral("kde-wallpaper-engine/kwin/kwe-occlusion.js"));
}
} // namespace

int main(int argc, char *argv[]) {
    QCoreApplication application(argc, argv);
    application.setApplicationName(QStringLiteral("kwe-occlusion-worker"));

    QCommandLineParser parser;
    parser.addHelpOption();
    QCommandLineOption socketOption(QStringLiteral("socket"),
                                    QStringLiteral("kwe-daemon RPC socket to report to"),
                                    QStringLiteral("path"));
    QCommandLineOption scriptOption(QStringLiteral("script"),
                                    QStringLiteral("KWin script to load (default: packaged kwe-occlusion.js)"),
                                    QStringLiteral("path"));
    QCommandLineOption debounceOption(QStringLiteral("debounce-ms"),
                                      QStringLiteral("Coalesce transitions for this long"),
                                      QStringLiteral("milliseconds"),
                                      QString::number(OcclusionBridge::DefaultDebounceMilliseconds));
    parser.addOptions({socketOption, scriptOption, debounceOption});
    parser.process(application);
    if (!parser.isSet(socketOption)) {
        fprintf(stderr, "kwe-occlusion-worker: --socket is required\n");
        return 2;
    }
    bool debounceOk = false;
    const int debounce = parser.value(debounceOption).toInt(&debounceOk);
    if (!debounceOk || debounce < 0 || debounce > 10000) {
        fprintf(stderr, "kwe-occlusion-worker: --debounce-ms must be 0..10000\n");
        return 2;
    }
    const QString scriptPath =
        parser.isSet(scriptOption) ? parser.value(scriptOption) : defaultScriptPath();
    if (scriptPath.isEmpty() || !QFileInfo::exists(scriptPath)) {
        fprintf(stderr, "event=occlusion.script_missing path=%s\n", qPrintable(scriptPath));
        return 66;
    }

    auto bus = QDBusConnection::sessionBus();
    if (!bus.isConnected()) {
        fprintf(stderr, "event=occlusion.session_bus_unavailable detail=%s\n",
                qPrintable(bus.lastError().message()));
        return 69;
    }
    OcclusionBridge bridge(parser.value(socketOption), debounce);
    if (!bus.registerObject(QString::fromLatin1(ObjectPath), &bridge, QDBusConnection::ExportScriptableSlots) ||
        !bus.registerService(QString::fromLatin1(ServiceName))) {
        fprintf(stderr, "event=occlusion.dbus_register_failed detail=%s\n",
                qPrintable(bus.lastError().message()));
        return 69;
    }

    KWinScript script(QFileInfo(scriptPath).absoluteFilePath());
    QDBusServiceWatcher watcher(QString::fromLatin1(KWinService), bus,
                                QDBusServiceWatcher::WatchForRegistration |
                                    QDBusServiceWatcher::WatchForUnregistration);
    QObject::connect(&watcher, &QDBusServiceWatcher::serviceRegistered, &bridge, [&] {
        fprintf(stderr, "event=occlusion.kwin_registered\n");
        script.load();
    });
    QObject::connect(&watcher, &QDBusServiceWatcher::serviceUnregistered, &bridge, [&] {
        fprintf(stderr, "event=occlusion.kwin_unregistered\n");
        bridge.reset();
    });
    if (!script.load()) {
        // KWin may still be starting (boot); the watcher retries on
        // registration. Meanwhile the daemon sees no verdict — uncovered.
        fprintf(stderr, "event=occlusion.waiting_for_kwin\n");
    }

    // Self-pipe: SIGTERM/SIGINT/SIGHUP unload the script before quitting.
    if (::socketpair(AF_UNIX, SOCK_STREAM, 0, signalPipe) != 0) {
        perror("socketpair");
        return 1;
    }
    QSocketNotifier notifier(signalPipe[0], QSocketNotifier::Read);
    QObject::connect(&notifier, &QSocketNotifier::activated, &application, [&] {
        char byte = 0;
        (void)::read(signalPipe[0], &byte, 1);
        application.quit();
    });
    for (const int signal : {SIGTERM, SIGINT, SIGHUP}) {
        struct sigaction action {};
        action.sa_handler = onSignal;
        action.sa_flags = SA_RESTART;
        ::sigaction(signal, &action, nullptr);
    }

    const int code = application.exec();
    script.unload();
    fprintf(stderr, "event=occlusion.stopped relayed=%d rejected=%d\n", bridge.relayedCount(),
            bridge.rejectedCount());
    return code;
}
