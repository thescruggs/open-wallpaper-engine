// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QByteArray>
#include <QHash>
#include <QJsonObject>
#include <QList>
#include <QLocalSocket>
#include <QObject>
#include <QTimer>
#include <functional>

// Daemon-backed global wallpaper settings (settings.get/set). Three knobs:
// audioOutput — whether wallpapers may play sound (the daemon respawns the
// live renderer on change), and pauseWhenCovered (F3) — whether rendering
// pauses while a fullscreen application covers every display (the daemon
// starts its KWin-backed detector while this is on), and
// pauseWhenDisplayOff (F4) — whether rendering pauses while every display
// is powered down (the daemon starts its display power helper while on).
// Plus the per-renderer-kind apply defaults (frame-rate limit and scaling
// for scene/video/web): the daemon owns them, every apply lane and playlist
// advance honours them, and the Settings page edits them through here.
// This client mirrors the effective values. Transport mirrors PlaylistClient:
// one request per connection, newline-delimited JSON, bounded queue with
// backoff so a toggle survives a daemon restart.
class SettingsClient final : public QObject {
    Q_OBJECT
    Q_PROPERTY(bool audioOutput READ audioOutput NOTIFY settingsChanged)
    Q_PROPERTY(bool pauseWhenCovered READ pauseWhenCovered NOTIFY settingsChanged)
    Q_PROPERTY(bool pauseWhenDisplayOff READ pauseWhenDisplayOff NOTIFY settingsChanged)
    Q_PROPERTY(bool loaded READ loaded NOTIFY settingsChanged)
    Q_PROPERTY(bool busy READ busy NOTIFY busyChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)

public:
    explicit SettingsClient(QString socketPath, QObject *parent = nullptr);
    bool audioOutput() const { return m_audioOutput; }
    bool pauseWhenCovered() const { return m_pauseWhenCovered; }
    bool pauseWhenDisplayOff() const { return m_pauseWhenDisplayOff; }
    bool loaded() const { return m_loaded; }
    bool busy() const { return m_busy; }
    QString errorMessage() const { return m_errorMessage; }

    Q_INVOKABLE void refresh();
    Q_INVOKABLE void setAudioOutput(bool enabled);
    Q_INVOKABLE void setPauseWhenCovered(bool enabled);
    Q_INVOKABLE void setPauseWhenDisplayOff(bool enabled);

    // Renderer defaults by kind ("scene" | "video" | "web"). Unknown kinds
    // read as the built-ins (30 fps, aspect) and are never written.
    Q_INVOKABLE int rendererFps(const QString &kind) const;
    Q_INVOKABLE QString rendererScaling(const QString &kind) const;
    Q_INVOKABLE void setRendererFps(const QString &kind, int fps);
    Q_INVOKABLE void setRendererScaling(const QString &kind, const QString &scaling);
    // True while every kind still carries the built-in defaults (used once,
    // to decide whether legacy per-user QSettings values should be migrated).
    bool rendererDefaultsAreBuiltIn() const;
    static bool isRendererKind(const QString &kind);
    static bool isRendererScaling(const QString &scaling);
    static constexpr int MinRendererFps = 1;
    static constexpr int MaxRendererFps = 240;
    static constexpr int BuiltInRendererFps = 30;

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
    bool applyRendererDefaults(const QJsonObject &record);
    void sendRendererDefault(const QString &kind, const QJsonObject &patch);

    struct KindDefaults {
        int fps = BuiltInRendererFps;
        QString scaling = QStringLiteral("aspect");
        bool operator==(const KindDefaults &other) const {
            return fps == other.fps && scaling == other.scaling;
        }
    };

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
    bool m_pauseWhenDisplayOff = false;
    QHash<QString, KindDefaults> m_rendererDefaults;
    bool m_loaded = false;
    bool m_busy = false;
    QString m_errorMessage;
};
