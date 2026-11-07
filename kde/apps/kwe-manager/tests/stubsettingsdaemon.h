// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QObject>

// Minimal daemon stand-in answering the settings.* wire protocol, including
// the per-kind renderer defaults record.
class StubSettingsDaemon final : public QObject {
    Q_OBJECT
public:
    StubSettingsDaemon() {
        connect(&m_server, &QLocalServer::newConnection, this, &StubSettingsDaemon::onConnection);
        reset();
    }
    bool listen(const QString &path) { return m_server.listen(path); }
    void reset() {
        audioOutput = true;
        pauseWhenCovered = false;
        pauseWhenDisplayOff = false;
        rendererDefaults = QJsonObject{
            {QStringLiteral("scene"), kind(30, QStringLiteral("aspect"))},
            {QStringLiteral("video"), kind(30, QStringLiteral("aspect"))},
            {QStringLiteral("web"), kind(30, QStringLiteral("aspect"))},
        };
        sets = 0;
        failNext = false;
        lastSetParams = {};
    }
    static QJsonObject kind(int fps, const QString &scaling) {
        return QJsonObject{{QStringLiteral("fps"), fps}, {QStringLiteral("scaling"), scaling}};
    }
    bool audioOutput = true;
    bool pauseWhenCovered = false;
    bool pauseWhenDisplayOff = false;
    QJsonObject rendererDefaults;
    int sets = 0;
    QJsonObject lastSetParams;
    bool failNext = false;

private:
    QLocalServer m_server;

    QJsonObject record() const {
        return QJsonObject{
            {QStringLiteral("audio_output"), audioOutput},
            {QStringLiteral("pause_when_covered"), pauseWhenCovered},
            {QStringLiteral("pause_when_display_off"), pauseWhenDisplayOff},
            {QStringLiteral("renderer_defaults"), rendererDefaults},
        };
    }

    void applyPatch(const QJsonObject &params) {
        if (params.contains(QStringLiteral("audio_output")))
            audioOutput = params.value(QStringLiteral("audio_output")).toBool();
        if (params.contains(QStringLiteral("pause_when_covered")))
            pauseWhenCovered = params.value(QStringLiteral("pause_when_covered")).toBool();
        if (params.contains(QStringLiteral("pause_when_display_off")))
            pauseWhenDisplayOff = params.value(QStringLiteral("pause_when_display_off")).toBool();
        const auto defaults = params.value(QStringLiteral("renderer_defaults")).toObject();
        for (auto it = defaults.constBegin(); it != defaults.constEnd(); ++it) {
            auto current = rendererDefaults.value(it.key()).toObject();
            const auto patch = it.value().toObject();
            if (patch.contains(QStringLiteral("fps")))
                current.insert(QStringLiteral("fps"), patch.value(QStringLiteral("fps")));
            if (patch.contains(QStringLiteral("scaling")))
                current.insert(QStringLiteral("scaling"), patch.value(QStringLiteral("scaling")));
            rendererDefaults.insert(it.key(), current);
        }
    }

    void onConnection() {
        auto *socket = m_server.nextPendingConnection();
        connect(socket, &QLocalSocket::readyRead, this, [this, socket] {
            const auto newline = socket->peek(64 * 1024).indexOf('\n');
            if (newline < 0)
                return;
            const auto request = QJsonDocument::fromJson(socket->read(newline + 1)).object();
            const auto method = request.value(QStringLiteral("method")).toString();
            QJsonObject result;
            bool ok = true;
            if (failNext) {
                failNext = false;
                ok = false;
                result.insert(QStringLiteral("error"), QStringLiteral("permissions_failed"));
            } else if (method == QStringLiteral("settings.get")) {
                result = record();
            } else if (method == QStringLiteral("settings.set")) {
                ++sets;
                lastSetParams = request.value(QStringLiteral("params")).toObject();
                applyPatch(lastSetParams);
                result = record();
            }
            socket->write(QJsonDocument(QJsonObject{
                              {QStringLiteral("version"), 1},
                              {QStringLiteral("id"), request.value(QStringLiteral("id"))},
                              {QStringLiteral("ok"), ok},
                              {QStringLiteral("result"), result},
                          }).toJson(QJsonDocument::Compact) + '\n');
            socket->flush();
        });
    }
};
