// SPDX-License-Identifier: GPL-3.0-or-later
#include "ledsyncclient.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonParseError>
#include <QRegularExpression>
#include <QVariantMap>

namespace {
// led.status carries the worker's status line (≤ 32 KiB) plus the daemon's
// own fields; leave generous headroom but stay bounded.
constexpr qsizetype MaxResponseBytes = 256 * 1024;
constexpr qsizetype MaxQueuedOperations = 8;
// The daemon lists ≤ 32 devices; never mirror more than this many.
constexpr int MaxDevices = 64;
constexpr int MaxTextLength = 80;
constexpr int FollowUpMilliseconds = 600;

const QStringList &knownModes() {
    static const QStringList modes{QStringLiteral("off"), QStringLiteral("ambient"),
                                   QStringLiteral("spectrum"), QStringLiteral("ambient+pulse")};
    return modes;
}

QString boundedText(const QJsonValue &value) {
    return value.toString().left(MaxTextLength);
}

QJsonObject merged(QJsonObject base, const QJsonObject &overlay) {
    for (auto it = overlay.begin(); it != overlay.end(); ++it)
        base.insert(it.key(), it.value());
    return base;
}
} // namespace

LedSyncClient::LedSyncClient(QString socketPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
    connect(&m_socket, &QLocalSocket::connected, this, &LedSyncClient::writeRequest);
    connect(&m_socket, &QLocalSocket::readyRead, this, &LedSyncClient::consumeResponse);
    connect(&m_socket, &QLocalSocket::errorOccurred, this, [this](QLocalSocket::LocalSocketError) {
        if (!m_inFlight)
            return;
        failTransport(tr("Could not reach the wallpaper service at %1: %2")
                          .arg(m_socketPath, m_socket.errorString()));
    });
    m_timeout.setSingleShot(true);
    m_timeout.setInterval(RequestTimeoutMilliseconds);
    connect(&m_timeout, &QTimer::timeout, this, [this] {
        if (m_inFlight)
            failTransport(tr("The wallpaper service did not answer in time."));
    });
    m_debounce.setSingleShot(true);
    m_debounce.setInterval(DebounceMilliseconds);
    connect(&m_debounce, &QTimer::timeout, this, &LedSyncClient::flush);
    m_reprobeCooldown.setSingleShot(true);
    connect(&m_reprobeCooldown, &QTimer::timeout, this, &LedSyncClient::reprobeChanged);
    m_followUp.setSingleShot(true);
    m_followUp.setInterval(FollowUpMilliseconds);
    connect(&m_followUp, &QTimer::timeout, this, &LedSyncClient::refresh);
}

bool LedSyncClient::enabled() const {
    const auto effective = merged(merged(m_settings, m_sentPatch), m_patch);
    return effective.value(QStringLiteral("enabled")).toBool(true);
}

QString LedSyncClient::mode() const {
    const auto effective = merged(merged(m_settings, m_sentPatch), m_patch);
    return effective.value(QStringLiteral("mode")).toString(QStringLiteral("ambient"));
}

int LedSyncClient::brightness() const {
    const auto effective = merged(merged(m_settings, m_sentPatch), m_patch);
    return effective.value(QStringLiteral("brightness")).toInt(100);
}

int LedSyncClient::rateHz() const {
    const auto effective = merged(merged(m_settings, m_sentPatch), m_patch);
    return effective.value(QStringLiteral("rate_hz")).toInt(10);
}

QStringList LedSyncClient::disabledDevices() const {
    const auto effective = merged(merged(m_settings, m_sentPatch), m_patch);
    QStringList ids;
    for (const auto &value : effective.value(QStringLiteral("disabled_devices")).toArray())
        ids.push_back(value.toString());
    return ids;
}

bool LedSyncClient::isDeviceEnabled(const QString &id) const {
    return !disabledDevices().contains(id);
}

void LedSyncClient::refresh() {
    if (m_inFlight && m_current.kind == Kind::Status)
        return;
    if (hasQueued(Kind::Status))
        return;
    enqueue(Pending{Kind::Status, {}});
}

void LedSyncClient::reprobe() {
    if (!canReprobe())
        return;
    m_reprobeInFlight = true;
    emit reprobeChanged();
    enqueue(Pending{Kind::Reprobe, {}});
}

void LedSyncClient::setEnabled(bool enabled) {
    editPatch(QStringLiteral("enabled"), enabled);
    flush();
}

void LedSyncClient::setMode(const QString &mode) {
    if (!knownModes().contains(mode))
        return;
    editPatch(QStringLiteral("mode"), mode);
    flush();
}

void LedSyncClient::setBrightness(int percent) {
    editPatch(QStringLiteral("brightness"), qBound(0, percent, 100));
}

void LedSyncClient::setRateHz(int hz) {
    editPatch(QStringLiteral("rate_hz"), qBound(1, hz, 20));
}

void LedSyncClient::setDeviceEnabled(const QString &id, bool enabled) {
    static const QRegularExpression deviceId(QStringLiteral("^[0-9a-f]{16}$"));
    if (!deviceId.match(id).hasMatch())
        return;
    // settings.set REPLACES the list, so start from the effective list: ids
    // of devices that are not connected right now stay disabled.
    auto ids = disabledDevices();
    if (enabled)
        ids.removeAll(id);
    else if (!ids.contains(id))
        ids.push_back(id);
    ids.sort();
    editPatch(QStringLiteral("disabled_devices"), QJsonArray::fromStringList(ids));
    flush();
}

void LedSyncClient::editPatch(const QString &key, const QJsonValue &value) {
    m_patch.insert(key, value);
    emit settingsChanged();
    m_debounce.start();
}

void LedSyncClient::flush() {
    m_debounce.stop();
    if (m_patch.isEmpty())
        return;
    // At most one settings.set waits in the queue: later edits merge into it.
    bool mergedIntoQueued = false;
    for (auto &queued : m_queue) {
        if (queued.kind != Kind::Set)
            continue;
        const auto led = merged(queued.params.value(QStringLiteral("led")).toObject(), m_patch);
        queued.params.insert(QStringLiteral("led"), led);
        mergedIntoQueued = true;
        break;
    }
    m_sentPatch = merged(m_sentPatch, m_patch);
    const auto patch = m_patch;
    m_patch = {};
    // Before enqueue: a refused connection can fail the request synchronously.
    setBusy(true);
    if (!mergedIntoQueued)
        enqueue(Pending{Kind::Set, QJsonObject{{QStringLiteral("led"), patch}}});
}

bool LedSyncClient::hasQueued(Kind kind) const {
    for (const auto &queued : m_queue) {
        if (queued.kind == kind)
            return true;
    }
    return false;
}

void LedSyncClient::enqueue(Pending pending) {
    if (m_inFlight) {
        if (m_queue.size() >= MaxQueuedOperations) {
            // Drop a status poll first; a fresher one follows anyway.
            bool dropped = false;
            for (qsizetype i = m_queue.size() - 1; i >= 0; --i) {
                if (m_queue.at(i).kind == Kind::Status) {
                    m_queue.removeAt(i);
                    dropped = true;
                    break;
                }
            }
            if (!dropped)
                return;
        }
        m_queue.push_back(std::move(pending));
        return;
    }
    begin(std::move(pending));
}

void LedSyncClient::begin(Pending pending) {
    m_current = std::move(pending);
    m_inFlight = true;
    m_socket.abort();
    m_buffer.clear();
    m_timeout.start();
    m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void LedSyncClient::writeRequest() {
    QString method;
    switch (m_current.kind) {
    case Kind::Status:
        method = QStringLiteral("led.status");
        break;
    case Kind::Reprobe:
        method = QStringLiteral("led.reprobe");
        break;
    case Kind::Set:
        method = QStringLiteral("settings.set");
        break;
    }
    QJsonObject request{
        {QStringLiteral("version"), 1},
        {QStringLiteral("id"), ++m_requestSerial},
        {QStringLiteral("method"), method},
    };
    if (m_current.kind == Kind::Set)
        request.insert(QStringLiteral("params"), m_current.params);
    m_socket.write(QJsonDocument(request).toJson(QJsonDocument::Compact) + '\n');
}

void LedSyncClient::consumeResponse() {
    if (!m_inFlight)
        return;
    m_buffer += m_socket.readAll();
    if (m_buffer.size() > MaxResponseBytes) {
        failTransport(tr("The wallpaper service returned more than the safety limit."));
        return;
    }
    const auto newline = m_buffer.indexOf('\n');
    if (newline < 0)
        return;
    QJsonParseError parseError;
    const auto document = QJsonDocument::fromJson(m_buffer.left(newline), &parseError);
    const Kind kind = m_current.kind;
    finishCurrent();
    if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
        handleFailure(kind, QStringLiteral("invalid_response"), {});
        drainQueue();
        return;
    }
    const auto response = document.object();
    const auto result = response.value(QStringLiteral("result")).toObject();
    if (!response.value(QStringLiteral("ok")).toBool()) {
        handleFailure(kind, result.value(QStringLiteral("error")).toString(),
                      result.value(QStringLiteral("detail")).toString());
        drainQueue();
        return;
    }
    switch (kind) {
    case Kind::Status:
        if (!result.value(QStringLiteral("state")).isString()) {
            handleFailure(kind, QStringLiteral("invalid_response"), {});
            break;
        }
        // A rejected write stays visible until the next accepted one.
        if (!m_errorFromSet)
            setError({});
        applyStatus(result);
        break;
    case Kind::Reprobe:
        if (!m_errorFromSet)
            setError({});
        applyReprobe(result);
        break;
    case Kind::Set: {
        // The effective record answered by settings.set is the truth.
        m_sentPatch = {};
        for (const auto &queued : m_queue) {
            if (queued.kind == Kind::Set)
                m_sentPatch = queued.params.value(QStringLiteral("led")).toObject();
        }
        if (result.value(QStringLiteral("led")).isObject()) {
            m_errorFromSet = false;
            setError({});
            applySettings(result.value(QStringLiteral("led")).toObject());
        } else {
            // A daemon that accepted the call but has no `led` record.
            emit settingsChanged();
            setSupport(QStringLiteral("unsupported"));
        }
        setBusy(hasQueued(Kind::Set) || !m_patch.isEmpty());
        // enabled on/off moves the state; pick it up promptly.
        refresh();
        break;
    }
    }
    drainQueue();
}

void LedSyncClient::finishCurrent() {
    m_timeout.stop();
    m_socket.disconnectFromServer();
    m_inFlight = false;
    if (m_current.kind == Kind::Reprobe) {
        m_reprobeInFlight = false;
        emit reprobeChanged();
    }
    m_current = {};
}

void LedSyncClient::handleFailure(Kind kind, const QString &error, const QString &detail) {
    if (error == QStringLiteral("unknown_method")) {
        // A daemon from before OpenRGB L3: no led.* methods.
        setError({});
        setSupport(QStringLiteral("unsupported"));
    } else if (error == QStringLiteral("led_unavailable")) {
        setError({});
        setSupport(QStringLiteral("unavailable"));
    } else if (error == QStringLiteral("invalid_response")) {
        setError(tr("The wallpaper service returned an invalid LED sync answer."));
    } else if (error == QStringLiteral("led_failed")) {
        setError(tr("The wallpaper service did not report LED sync in time. It will be asked again."));
    } else if (kind == Kind::Set) {
        setError(detail.isEmpty()
                     ? tr("The LED sync setting was not changed (%1).").arg(error)
                     : tr("The LED sync setting was not changed: %1").arg(detail));
    } else {
        setError(detail.isEmpty() ? error : tr("%1: %2").arg(error, detail));
    }
    m_errorFromSet = kind == Kind::Set;
    if (kind == Kind::Set) {
        // Rejected as a whole: fall back to the daemon's record and re-read.
        m_sentPatch = {};
        for (const auto &queued : m_queue) {
            if (queued.kind == Kind::Set)
                m_sentPatch = queued.params.value(QStringLiteral("led")).toObject();
        }
        emit settingsChanged();
        setBusy(hasQueued(Kind::Set) || !m_patch.isEmpty());
        refresh();
    }
}

void LedSyncClient::failTransport(const QString &error) {
    const Kind kind = m_current.kind;
    m_timeout.stop();
    m_socket.abort();
    m_inFlight = false;
    m_current = {};
    if (kind == Kind::Reprobe) {
        m_reprobeInFlight = false;
        emit reprobeChanged();
    }
    if (kind == Kind::Set) {
        // Not applied; the controls go back to the last confirmed values.
        m_sentPatch = {};
        for (const auto &queued : m_queue) {
            if (queued.kind == Kind::Set)
                m_sentPatch = queued.params.value(QStringLiteral("led")).toObject();
        }
        emit settingsChanged();
        setBusy(hasQueued(Kind::Set) || !m_patch.isEmpty());
    }
    m_errorFromSet = false;
    setError(error);
    setSupport(QStringLiteral("unreachable"));
    drainQueue();
}

void LedSyncClient::drainQueue() {
    if (m_inFlight || m_queue.isEmpty())
        return;
    begin(m_queue.takeFirst());
}

void LedSyncClient::applyStatus(const QJsonObject &status) {
    setSupport(QStringLiteral("ready"));
    m_state = boundedText(status.value(QStringLiteral("state")));
    m_reason = boundedText(status.value(QStringLiteral("reason")));
    m_paused = status.value(QStringLiteral("paused")).toBool(false);

    const auto worker = status.value(QStringLiteral("worker_status"));
    m_hasWorkerStatus = worker.isObject();
    const auto workerStatus = worker.toObject();
    const auto server = workerStatus.value(QStringLiteral("server")).toObject();
    m_protocolVersion = server.value(QStringLiteral("protocol")).toInt(0);
    m_sdkPort = server.value(QStringLiteral("port")).toInt(0);
    m_audioFresh =
        workerStatus.value(QStringLiteral("audio")).toObject().value(QStringLiteral("fresh")).toBool(false);
    m_devicesTruncated = workerStatus.value(QStringLiteral("devices_truncated")).toBool(false);
    QVariantList devices;
    for (const auto &value : workerStatus.value(QStringLiteral("devices")).toArray()) {
        if (devices.size() >= MaxDevices) {
            m_devicesTruncated = true;
            break;
        }
        const auto device = value.toObject();
        const auto id = boundedText(device.value(QStringLiteral("id")));
        if (id.isEmpty())
            continue;
        QVariantMap entry;
        entry.insert(QStringLiteral("id"), id);
        auto name = boundedText(device.value(QStringLiteral("name")));
        entry.insert(QStringLiteral("name"), name.isEmpty() ? tr("Unnamed device") : name);
        entry.insert(QStringLiteral("vendor"), boundedText(device.value(QStringLiteral("vendor"))));
        entry.insert(QStringLiteral("leds"), device.value(QStringLiteral("leds")).toInt(0));
        entry.insert(QStringLiteral("controlled"),
                     device.value(QStringLiteral("controlled")).toBool(false));
        devices.push_back(entry);
    }
    m_devices = devices;
    m_deviceCount = workerStatus.value(QStringLiteral("device_count")).toInt(int(devices.size()));
    m_drivenCount = workerStatus.value(QStringLiteral("driven_count")).toInt(-1);
    emit statusChanged();

    if (status.value(QStringLiteral("settings")).isObject())
        applySettings(status.value(QStringLiteral("settings")).toObject());
}

void LedSyncClient::applySettings(const QJsonObject &led) {
    QJsonObject settings;
    for (const auto &key : {QStringLiteral("enabled"), QStringLiteral("mode"),
                            QStringLiteral("brightness"), QStringLiteral("rate_hz"),
                            QStringLiteral("disabled_devices")}) {
        if (led.contains(key))
            settings.insert(key, led.value(key));
    }
    if (settings == m_settings)
        return;
    m_settings = settings;
    emit settingsChanged();
}

void LedSyncClient::applyReprobe(const QJsonObject &result) {
    if (result.value(QStringLiteral("status")).isObject())
        applyStatus(result.value(QStringLiteral("status")).toObject());
    const bool accepted = result.value(QStringLiteral("accepted")).toBool(false);
    const auto reason = result.value(QStringLiteral("reason")).toString();
    int cooldown = 0;
    if (accepted) {
        // Matches the daemon's one-accepted-call-per-2 s limit, so a second
        // click is never answered with rate_limited.
        cooldown = ReprobeCooldownMilliseconds;
        // The new state arrives in a later led.status.
        m_followUp.start();
    } else if (reason == QStringLiteral("rate_limited")) {
        cooldown = result.value(QStringLiteral("retry_after_ms")).toInt(ReprobeCooldownMilliseconds);
    } else if (reason == QStringLiteral("worker_restarting")) {
        // Newer daemons say when the helper will be back; older ones do not.
        cooldown = result.value(QStringLiteral("retry_after_ms")).toInt(1000);
        m_followUp.start();
    }
    if (cooldown > 0) {
        m_reprobeCooldown.start(qBound(250, cooldown, MaxReprobeCooldownMilliseconds));
        emit reprobeChanged();
    }
}

void LedSyncClient::setBusy(bool busy) {
    if (m_busy == busy)
        return;
    m_busy = busy;
    emit busyChanged();
}

void LedSyncClient::setError(const QString &error) {
    if (m_errorMessage == error)
        return;
    m_errorMessage = error;
    emit errorMessageChanged();
}

void LedSyncClient::setSupport(const QString &support) {
    if (m_support == support)
        return;
    m_support = support;
    emit supportChanged();
}
