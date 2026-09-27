// SPDX-License-Identifier: GPL-3.0-or-later
#include "settingsclient.h"

#include <QFileInfo>
#include <QJsonDocument>
#include <QJsonParseError>

namespace {
constexpr int InitialRetryMilliseconds = 5000;
constexpr int MaximumRetryMilliseconds = 30000;
constexpr qsizetype MaxResponseBytes = 64 * 1024;
constexpr qsizetype MaxQueuedOperations = 8;
}

SettingsClient::SettingsClient(QString socketPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
    connect(&m_socket, &QLocalSocket::connected, this, &SettingsClient::writeRequest);
    connect(&m_socket, &QLocalSocket::readyRead, this, &SettingsClient::consumeResponse);
    connect(&m_socket, &QLocalSocket::errorOccurred, this, [this](QLocalSocket::LocalSocketError) {
        failCurrent(tr("Could not connect to the wallpaper service at %1: %2")
                        .arg(m_socketPath, m_socket.errorString()));
    });
    m_retryTimer.setSingleShot(true);
    connect(&m_retryTimer, &QTimer::timeout, this, [this] {
        if (m_inFlight)
            return;
        if (!m_queue.isEmpty())
            begin(m_queue.takeFirst());
    });
    refresh();
}

void SettingsClient::refresh() {
    send(Pending{QStringLiteral("settings.get"), {}, false});
}

void SettingsClient::setAudioOutput(bool enabled) {
    // Optimistic: the UI switch reflects the choice immediately; the daemon's
    // reply (or the drained queue after an outage) is authoritative.
    if (m_audioOutput != enabled || !m_loaded) {
        m_audioOutput = enabled;
        emit settingsChanged();
    }
    send(Pending{QStringLiteral("settings.set"),
                 QJsonObject{{QStringLiteral("audio_output"), enabled}}, true});
}

void SettingsClient::setPauseWhenCovered(bool enabled) {
    if (m_pauseWhenCovered != enabled || !m_loaded) {
        m_pauseWhenCovered = enabled;
        emit settingsChanged();
    }
    send(Pending{QStringLiteral("settings.set"),
                 QJsonObject{{QStringLiteral("pause_when_covered"), enabled}}, true});
}

void SettingsClient::setPauseWhenDisplayOff(bool enabled) {
    if (m_pauseWhenDisplayOff != enabled || !m_loaded) {
        m_pauseWhenDisplayOff = enabled;
        emit settingsChanged();
    }
    send(Pending{QStringLiteral("settings.set"),
                 QJsonObject{{QStringLiteral("pause_when_display_off"), enabled}}, true});
}

void SettingsClient::send(Pending pending) {
    if (m_inFlight) {
        if (m_queue.size() >= MaxQueuedOperations)
            m_queue.removeLast();
        m_queue.push_back(std::move(pending));
        return;
    }
    begin(std::move(pending));
}

void SettingsClient::begin(Pending pending) {
    m_current = std::move(pending);
    m_inFlight = true;
    setBusy(true);
    m_socket.abort();
    m_buffer.clear();
    m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void SettingsClient::writeRequest() {
    QJsonObject request{
        {QStringLiteral("version"), 1},
        {QStringLiteral("id"), ++m_requestSerial},
        {QStringLiteral("method"), m_current.method},
    };
    if (m_current.hasParams)
        request.insert(QStringLiteral("params"), m_current.params);
    m_socket.write(QJsonDocument(request).toJson(QJsonDocument::Compact) + '\n');
}

void SettingsClient::consumeResponse() {
    m_buffer += m_socket.readAll();
    if (m_buffer.size() > MaxResponseBytes) {
        failCurrent(tr("The wallpaper service returned more than the safety limit."));
        return;
    }
    const auto newline = m_buffer.indexOf('\n');
    if (newline < 0)
        return;
    QJsonParseError parseError;
    const auto document = QJsonDocument::fromJson(m_buffer.left(newline), &parseError);
    m_socket.disconnectFromServer();
    m_inFlight = false;
    m_current = {};
    if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
        setError(tr("The wallpaper service returned an invalid response."));
        setBusy(!m_queue.isEmpty());
        drainQueue();
        return;
    }
    const auto response = document.object();
    const auto result = response.value(QStringLiteral("result")).toObject();
    if (response.value(QStringLiteral("ok")).toBool()) {
        m_retryDelayMilliseconds = InitialRetryMilliseconds;
        setError({});
        applyResult(result);
    } else {
        const auto error = result.value(QStringLiteral("error")).toString();
        const auto detail = result.value(QStringLiteral("detail")).toString();
        setError(detail.isEmpty() ? error : tr("%1: %2").arg(error, detail));
    }
    setBusy(!m_queue.isEmpty());
    drainQueue();
}

void SettingsClient::applyResult(const QJsonObject &result) {
    const bool enabled =
        result.value(QStringLiteral("audio_output")).toBool(m_audioOutput);
    const bool pause =
        result.value(QStringLiteral("pause_when_covered")).toBool(m_pauseWhenCovered);
    const bool pauseDisplayOff = result.value(QStringLiteral("pause_when_display_off"))
                                     .toBool(m_pauseWhenDisplayOff);
    const bool changed = !m_loaded || enabled != m_audioOutput || pause != m_pauseWhenCovered ||
                         pauseDisplayOff != m_pauseWhenDisplayOff;
    m_loaded = true;
    m_audioOutput = enabled;
    m_pauseWhenCovered = pause;
    m_pauseWhenDisplayOff = pauseDisplayOff;
    if (changed)
        emit settingsChanged();
}

void SettingsClient::failCurrent(const QString &error) {
    m_socket.abort();
    m_inFlight = false;
    if (!m_current.method.isEmpty()) {
        // Re-queue the failed operation at the front so a toggle made while
        // the daemon restarts is not lost.
        if (m_queue.size() >= MaxQueuedOperations)
            m_queue.removeLast();
        m_queue.push_front(std::move(m_current));
        m_current = {};
    }
    setError(error);
    // The queued operation retries in the background; the switch stays
    // usable meanwhile (later toggles queue behind it, bounded).
    setBusy(false);
    retryLater();
}

void SettingsClient::drainQueue() {
    if (m_inFlight || m_queue.isEmpty())
        return;
    begin(m_queue.takeFirst());
}

void SettingsClient::retryLater() {
    if (m_retryTimer.isActive())
        return;
    m_retryTimer.start(m_retryDelayMilliseconds);
    m_retryDelayMilliseconds = qMin(m_retryDelayMilliseconds * 2, MaximumRetryMilliseconds);
}

void SettingsClient::setBusy(bool busy) {
    if (m_busy == busy)
        return;
    m_busy = busy;
    emit busyChanged();
}

void SettingsClient::setError(const QString &error) {
    if (m_errorMessage == error)
        return;
    m_errorMessage = error;
    emit errorMessageChanged();
}
