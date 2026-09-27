// SPDX-License-Identifier: GPL-3.0-or-later
#include "occlusionbridge.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonParseError>

namespace {
constexpr qsizetype MaxReportBytes = 16 * 1024;
constexpr qsizetype MaxResponseBytes = 64 * 1024;
constexpr int MaxOutputs = 64;
constexpr int ReplyTimeoutMilliseconds = 3000;

bool readNames(const QJsonValue &value, QStringList &names) {
    if (!value.isArray())
        return false;
    const auto array = value.toArray();
    if (array.size() > MaxOutputs)
        return false;
    names.clear();
    for (const auto &entry : array) {
        if (!entry.isString())
            return false;
        const auto name = entry.toString();
        if (name.isEmpty() || name.size() > 128)
            return false;
        if (!names.contains(name))
            names.push_back(name);
    }
    names.sort();
    return true;
}
} // namespace

OcclusionBridge::OcclusionBridge(QString socketPath, int debounceMilliseconds, QObject *parent,
                                 QString method, QString subsetKey)
    : QObject(parent), m_socketPath(std::move(socketPath)), m_method(std::move(method)),
      m_subsetKey(std::move(subsetKey)) {
    m_debounce.setSingleShot(true);
    m_debounce.setInterval(debounceMilliseconds);
    connect(&m_debounce, &QTimer::timeout, this, &OcclusionBridge::flush);
    connect(&m_socket, &QLocalSocket::connected, this, [this] {
        const QJsonObject request{
            {QStringLiteral("version"), 1},
            {QStringLiteral("id"), ++m_serial},
            {QStringLiteral("method"), m_method},
            {QStringLiteral("params"),
             QJsonObject{{QStringLiteral("outputs"), QJsonArray::fromStringList(m_outputs)},
                         {m_subsetKey, QJsonArray::fromStringList(m_covered)}}},
        };
        m_socket.write(QJsonDocument(request).toJson(QJsonDocument::Compact) + '\n');
    });
    connect(&m_socket, &QLocalSocket::readyRead, this, [this] {
        m_buffer += m_socket.readAll();
        if (m_buffer.size() > MaxResponseBytes) {
            finish(false, QStringLiteral("daemon response exceeded the safety limit"));
            return;
        }
        const auto newline = m_buffer.indexOf('\n');
        if (newline < 0)
            return;
        QJsonParseError parseError;
        const auto document = QJsonDocument::fromJson(m_buffer.left(newline), &parseError);
        if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
            finish(false, QStringLiteral("daemon returned an invalid response"));
            return;
        }
        const auto response = document.object();
        if (response.value(QStringLiteral("ok")).toBool()) {
            finish(true, {});
        } else {
            const auto result = response.value(QStringLiteral("result")).toObject();
            finish(false, result.value(QStringLiteral("detail"))
                              .toString(result.value(QStringLiteral("error")).toString()));
        }
    });
    connect(&m_socket, &QLocalSocket::errorOccurred, this, [this](QLocalSocket::LocalSocketError) {
        if (m_inFlight)
            finish(false, m_socket.errorString());
    });
}

void OcclusionBridge::Report(const QString &json) {
    const QStringList outputs = m_outputs;
    const QStringList covered = m_covered;
    if (!applyReport(json)) {
        ++m_rejected;
        return;
    }
    // An identical view (the script re-evaluated without a transition, or a
    // reload after a KWin restart) relays nothing.
    if (outputs != m_outputs || covered != m_covered)
        scheduleFlush();
}

bool OcclusionBridge::applyReport(const QString &json) {
    if (json.size() > MaxReportBytes)
        return false;
    QJsonParseError parseError;
    const auto document = QJsonDocument::fromJson(json.toUtf8(), &parseError);
    if (parseError.error != QJsonParseError::NoError || !document.isObject())
        return false;
    const auto object = document.object();
    QStringList outputs;
    QStringList covered;
    if (!readNames(object.value(QStringLiteral("outputs")), outputs) ||
        !readNames(object.value(m_subsetKey), covered))
        return false;
    if (outputs == m_outputs && covered == m_covered)
        return true; // identical view: nothing to relay
    m_outputs = outputs;
    m_covered = covered;
    emit stateChanged();
    return true;
}

void OcclusionBridge::reset() {
    if (m_outputs.isEmpty() && m_covered.isEmpty())
        return;
    m_outputs.clear();
    m_covered.clear();
    emit stateChanged();
    scheduleFlush();
}

void OcclusionBridge::scheduleFlush() {
    m_dirty = true;
    // A change during an in-flight request is picked up by finish().
    if (!m_inFlight)
        m_debounce.start();
}

void OcclusionBridge::flush() {
    if (m_inFlight || !m_dirty)
        return;
    m_dirty = false;
    m_inFlight = true;
    m_buffer.clear();
    m_socket.abort();
    m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
    // A daemon that accepts but never answers must not wedge the relay.
    QTimer::singleShot(ReplyTimeoutMilliseconds, this, [this, serial = m_serial + 1] {
        if (m_inFlight && m_serial == serial)
            finish(false, QStringLiteral("daemon reply timed out"));
    });
}

void OcclusionBridge::finish(bool ok, const QString &error) {
    if (!m_inFlight)
        return;
    m_inFlight = false;
    m_socket.abort();
    m_buffer.clear();
    if (ok) {
        ++m_relayed;
        m_lastError.clear();
    } else {
        m_lastError = error;
        fprintf(stderr, "event=occlusion.relay_failed detail=%s\n", qPrintable(error));
    }
    emit relayed(ok);
    if (m_dirty)
        m_debounce.start();
}
