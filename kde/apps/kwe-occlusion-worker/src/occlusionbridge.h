// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QByteArray>
#include <QJsonObject>
#include <QLocalSocket>
#include <QObject>
#include <QStringList>
#include <QTimer>

// D-Bus object the packaged KWin script calls (`Report(json)`) and the relay
// that forwards the debounced verdict to kwe-daemon as `occlusion.report`.
// One request per connection (newline-delimited JSON, like every other
// daemon client); a change arriving while a request is in flight is sent
// once that request completes (latest wins, never queued).
class OcclusionBridge final : public QObject {
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.kde.kwe.Occlusion1")

public:
    static constexpr int DefaultDebounceMilliseconds = 250;

    explicit OcclusionBridge(QString socketPath, int debounceMilliseconds = DefaultDebounceMilliseconds,
                             QObject *parent = nullptr);

    QStringList outputs() const { return m_outputs; }
    QStringList covered() const { return m_covered; }
    int relayedCount() const { return m_relayed; }
    int rejectedCount() const { return m_rejected; }
    QString lastError() const { return m_lastError; }

    // Parses one report; returns false (and changes nothing) when the JSON
    // is not the {"outputs":[...],"covered":[...]} shape. Exposed for tests.
    bool applyReport(const QString &json);

    // Forget the detector's view (KWin went away): relays an uncovered
    // desktop so the wallpaper never stays frozen behind a dead compositor.
    void reset();

public Q_SLOTS:
    Q_SCRIPTABLE void Report(const QString &json);

Q_SIGNALS:
    void stateChanged();
    void relayed(bool ok);

private:
    void scheduleFlush();
    void flush();
    void finish(bool ok, const QString &error);

    QString m_socketPath;
    QStringList m_outputs;
    QStringList m_covered;
    QTimer m_debounce;
    QLocalSocket m_socket;
    QByteArray m_buffer;
    bool m_inFlight = false;
    bool m_dirty = false;
    int m_relayed = 0;
    int m_rejected = 0;
    int m_serial = 0;
    QString m_lastError;
};
