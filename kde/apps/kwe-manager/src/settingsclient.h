// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QByteArray>
#include <QJsonObject>
#include <QList>
#include <QLocalSocket>
#include <QObject>
#include <QTimer>
#include <functional>

// Daemon-backed global wallpaper settings (settings.get/set). Two knobs:
// audioOutput — whether wallpapers may play sound (the daemon respawns the
// live renderer on change), and pauseWhenCovered (F3) — whether rendering
// pauses while a fullscreen application covers every display (the daemon
// starts its KWin-backed detector while this is on).
// This client mirrors the effective values. Transport mirrors PlaylistClient:
// one request per connection, newline-delimited JSON, bounded queue with
// backoff so a toggle survives a daemon restart.
class SettingsClient final : public QObject {
    Q_OBJECT
    Q_PROPERTY(bool audioOutput READ audioOutput NOTIFY settingsChanged)
    Q_PROPERTY(bool pauseWhenCovered READ pauseWhenCovered NOTIFY settingsChanged)
    Q_PROPERTY(bool loaded READ loaded NOTIFY settingsChanged)
    Q_PROPERTY(bool busy READ busy NOTIFY busyChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)

public:
    explicit SettingsClient(QString socketPath, QObject *parent = nullptr);
    bool audioOutput() const { return m_audioOutput; }
    bool pauseWhenCovered() const { return m_pauseWhenCovered; }
    bool loaded() const { return m_loaded; }
    bool busy() const { return m_busy; }
    QString errorMessage() const { return m_errorMessage; }

    Q_INVOKABLE void refresh();
    Q_INVOKABLE void setAudioOutput(bool enabled);
    Q_INVOKABLE void setPauseWhenCovered(bool enabled);

signals:
    void settingsChanged();
    void busyChanged();
    void errorMessageChanged();

private:
    struct Pending {
        QString method;
        QJsonObject params;
        bool hasParams = false;
    };

    void send(Pending pending);
    void begin(Pending pending);
    void writeRequest();
    void consumeResponse();
    void failCurrent(const QString &error);
    void drainQueue();
    void retryLater();
    void setBusy(bool busy);
    void setError(const QString &error);
    void applyResult(const QJsonObject &result);

    QString m_socketPath;
    QLocalSocket m_socket;
    QByteArray m_buffer;
    QList<Pending> m_queue;
    Pending m_current;
    bool m_inFlight = false;
    QTimer m_retryTimer;
    int m_retryDelayMilliseconds = 5000;
    int m_requestSerial = 0;
    bool m_audioOutput = true;
    bool m_pauseWhenCovered = false;
    bool m_loaded = false;
    bool m_busy = false;
    QString m_errorMessage;
};
