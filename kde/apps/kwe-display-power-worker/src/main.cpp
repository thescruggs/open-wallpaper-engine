// SPDX-License-Identifier: GPL-3.0-or-later
// kwe-display-power-worker (F4): a daemon-spawned helper that watches the
// compositor's display power state (DPMS) through libkscreen and relays it
// to kwe-daemon as `display.power.report`. It runs only while the
// pause-when-display-off setting is on; the daemon terminates it (SIGTERM,
// then SIGKILL) when the policy is switched off and PR_SET_PDEATHSIG covers
// a crashed daemon. It is a windowless GUI client because the power state
// is only published on the compositor connection; it never creates a
// surface.
#include "../../kwe-occlusion-worker/src/occlusionbridge.h"
#include "displaypowerwatcher.h"

#include <QCommandLineParser>
#include <QGuiApplication>
#include <QSocketNotifier>

#include <csignal>
#include <cstdio>
#include <sys/socket.h>
#include <unistd.h>

namespace {
int signalPipe[2] = {-1, -1};

void onSignal(int) {
    const char byte = 1;
    (void)::write(signalPipe[1], &byte, 1);
}
} // namespace

int main(int argc, char *argv[]) {
    // The daemon is a systemd user unit: the session type is not always in
    // its environment, the compositor socket is.
    if (qEnvironmentVariableIsEmpty("QT_QPA_PLATFORM") &&
        !qEnvironmentVariableIsEmpty("WAYLAND_DISPLAY"))
        qputenv("QT_QPA_PLATFORM", "wayland");
    QCoreApplication::setAttribute(Qt::AA_DisableSessionManager);
    QGuiApplication application(argc, argv);
    application.setApplicationName(QStringLiteral("kwe-display-power-worker"));
    application.setQuitOnLastWindowClosed(false);

    QCommandLineParser parser;
    parser.addHelpOption();
    QCommandLineOption socketOption(QStringLiteral("socket"),
                                    QStringLiteral("kwe-daemon RPC socket to report to"),
                                    QStringLiteral("path"));
    QCommandLineOption debounceOption(QStringLiteral("debounce-ms"),
                                      QStringLiteral("Coalesce transitions for this long"),
                                      QStringLiteral("milliseconds"),
                                      QString::number(OcclusionBridge::DefaultDebounceMilliseconds));
    parser.addOptions({socketOption, debounceOption});
    parser.process(application);
    if (!parser.isSet(socketOption)) {
        fprintf(stderr, "kwe-display-power-worker: --socket is required\n");
        return 2;
    }
    bool debounceOk = false;
    const int debounce = parser.value(debounceOption).toInt(&debounceOk);
    if (!debounceOk || debounce < 0 || debounce > 10000) {
        fprintf(stderr, "kwe-display-power-worker: --debounce-ms must be 0..10000\n");
        return 2;
    }

    OcclusionBridge bridge(parser.value(socketOption), debounce, nullptr,
                           QStringLiteral("display.power.report"), QStringLiteral("asleep"));
    DisplayPowerWatcher watcher;
    QObject::connect(&watcher, &DisplayPowerWatcher::reportChanged, &bridge,
                     [&bridge](const QString &json) {
                         fprintf(stderr, "event=display_power.state report=%s\n", qPrintable(json));
                         bridge.Report(json);
                     });
    fprintf(stderr, "event=display_power.started supported=%d\n", int(watcher.supported()));

    // Self-pipe: SIGTERM/SIGINT/SIGHUP leave through the event loop.
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
    fprintf(stderr, "event=display_power.stopped relayed=%d rejected=%d\n", bridge.relayedCount(),
            bridge.rejectedCount());
    return code;
}
