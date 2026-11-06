// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QByteArray>
#include <QJsonObject>
#include <QList>
#include <QLocalSocket>
#include <QObject>
#include <QStringList>
#include <QTimer>
#include <QVariantList>

// OpenRGB L4: the manager's view of daemon LED sync (docs/SUPERVISOR_API_V1.md
// "LED sync"). Reads `led.status` (polled by the Settings page only while it
// is visible), calls `led.reprobe` ("Check again"), and writes LED settings
// through `settings.set {"led": {...only the changed fields...}}`.
//
// The daemon is the source of truth: every answer (status or settings.set)
// replaces the mirrored settings. Edits are applied optimistically so the
// controls do not jump back mid-drag, merged into one pending patch and sent
// after a short debounce so slider drags do not flood the daemon.
//
// Transport mirrors SettingsClient: one request per connection,
// newline-delimited JSON, a bounded queue, a per-request timeout.
class LedSyncClient final : public QObject {
    Q_OBJECT
    // "loading" (no answer yet), "ready", "unsupported" (older daemon without
    // the led.* methods), "unavailable" (daemon without its supervisor),
    // "unreachable" (no daemon answer).
    Q_PROPERTY(QString support READ support NOTIFY supportChanged)
    Q_PROPERTY(bool busy READ busy NOTIFY busyChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)

    // Daemon state (`led.status` state / reason / paused).
    Q_PROPERTY(QString state READ state NOTIFY statusChanged)
    Q_PROPERTY(QString reason READ reason NOTIFY statusChanged)
    Q_PROPERTY(bool paused READ paused NOTIFY statusChanged)
    // Worker-side details; meaningful only while hasWorkerStatus.
    Q_PROPERTY(bool hasWorkerStatus READ hasWorkerStatus NOTIFY statusChanged)
    Q_PROPERTY(int protocolVersion READ protocolVersion NOTIFY statusChanged)
    Q_PROPERTY(int sdkPort READ sdkPort NOTIFY statusChanged)
    Q_PROPERTY(bool audioFresh READ audioFresh NOTIFY statusChanged)
    Q_PROPERTY(int deviceCount READ deviceCount NOTIFY statusChanged)
    // Devices the worker drives right now (`driven_count`); -1 when the
    // worker does not report it (older worker).
    Q_PROPERTY(int drivenCount READ drivenCount NOTIFY statusChanged)
    Q_PROPERTY(bool devicesTruncated READ devicesTruncated NOTIFY statusChanged)
    // [{id, name, vendor, leds, controlled}], at most what the daemon lists.
    Q_PROPERTY(QVariantList devices READ devices NOTIFY statusChanged)

    // Effective settings (daemon answer, overlaid by not-yet-acknowledged
    // local edits).
    Q_PROPERTY(bool enabled READ enabled NOTIFY settingsChanged)
    Q_PROPERTY(QString mode READ mode NOTIFY settingsChanged)
    Q_PROPERTY(int brightness READ brightness NOTIFY settingsChanged)
    Q_PROPERTY(int rateHz READ rateHz NOTIFY settingsChanged)
    Q_PROPERTY(QStringList disabledDevices READ disabledDevices NOTIFY settingsChanged)

    // "Check again": false while a reprobe is in flight or the daemon's rate
    // limit (or our own matching cooldown) is running.
    Q_PROPERTY(bool canReprobe READ canReprobe NOTIFY reprobeChanged)

public:
    static constexpr int DebounceMilliseconds = 300;
    static constexpr int RequestTimeoutMilliseconds = 5000;
    static constexpr int ReprobeCooldownMilliseconds = 2000;
    static constexpr int MaxReprobeCooldownMilliseconds = 10000;

    explicit LedSyncClient(QString socketPath, QObject *parent = nullptr);

    QString support() const { return m_support; }
    bool busy() const { return m_busy; }
    QString errorMessage() const { return m_errorMessage; }
    QString state() const { return m_state; }
    QString reason() const { return m_reason; }
    bool paused() const { return m_paused; }
    bool hasWorkerStatus() const { return m_hasWorkerStatus; }
    int protocolVersion() const { return m_protocolVersion; }
    int sdkPort() const { return m_sdkPort; }
    bool audioFresh() const { return m_audioFresh; }
    int deviceCount() const { return m_deviceCount; }
    int drivenCount() const { return m_drivenCount; }
    bool devicesTruncated() const { return m_devicesTruncated; }
    QVariantList devices() const { return m_devices; }
    bool enabled() const;
    QString mode() const;
    int brightness() const;
    int rateHz() const;
    QStringList disabledDevices() const;
    bool canReprobe() const { return !m_reprobeInFlight && !m_reprobeCooldown.isActive(); }

    // Polls led.status; a no-op while a status request is already pending.
    Q_INVOKABLE void refresh();
    Q_INVOKABLE void reprobe();
    Q_INVOKABLE void setEnabled(bool enabled);
    Q_INVOKABLE void setMode(const QString &mode);
    Q_INVOKABLE void setBrightness(int percent);
    Q_INVOKABLE void setRateHz(int hz);
    Q_INVOKABLE void setDeviceEnabled(const QString &id, bool enabled);
    Q_INVOKABLE bool isDeviceEnabled(const QString &id) const;
    // Sends any debounced edit now (tests; the page also calls it on hide).
    Q_INVOKABLE void flush();

signals:
    void supportChanged();
    void busyChanged();
    void errorMessageChanged();
    void statusChanged();
    void settingsChanged();
    void reprobeChanged();

private:
    enum class Kind { Status, Reprobe, Set };
    struct Pending {
        Kind kind = Kind::Status;
        QJsonObject params;
    };

    void enqueue(Pending pending);
    void begin(Pending pending);
    void writeRequest();
    void consumeResponse();
    void finishCurrent();
    void failTransport(const QString &error);
    void drainQueue();
    void setBusy(bool busy);
    void setError(const QString &error);
    void setSupport(const QString &support);
    void applyStatus(const QJsonObject &status);
    void applySettings(const QJsonObject &led);
    void applyReprobe(const QJsonObject &result);
    void handleFailure(Kind kind, const QString &error, const QString &detail);
    void editPatch(const QString &key, const QJsonValue &value);
    bool hasQueued(Kind kind) const;

    QString m_socketPath;
    QLocalSocket m_socket;
    QByteArray m_buffer;
    QList<Pending> m_queue;
    Pending m_current;
    bool m_inFlight = false;
    int m_requestSerial = 0;
    QTimer m_timeout;
    QTimer m_debounce;
    QTimer m_reprobeCooldown;
    QTimer m_followUp;
    bool m_reprobeInFlight = false;
    // The current errorMessage is a rejected settings.set (kept until the
    // next accepted write, not cleared by status polls).
    bool m_errorFromSet = false;

    QString m_support = QStringLiteral("loading");
    bool m_busy = false;
    QString m_errorMessage;

    QString m_state;
    QString m_reason;
    bool m_paused = false;
    bool m_hasWorkerStatus = false;
    int m_protocolVersion = 0;
    int m_sdkPort = 0;
    bool m_audioFresh = false;
    int m_deviceCount = 0;
    int m_drivenCount = -1;
    bool m_devicesTruncated = false;
    QVariantList m_devices;

    // Daemon-confirmed settings.
    QJsonObject m_settings;
    // Edits not yet sent (debouncing) and edits sent but not yet answered;
    // both overlay m_settings so the controls stay where the user put them.
    QJsonObject m_patch;
    QJsonObject m_sentPatch;
};
