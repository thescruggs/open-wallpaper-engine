// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/settingsclient.h"

#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QTemporaryDir>
#include <QtTest>

// Minimal daemon stand-in answering the settings.* wire protocol.
class StubDaemon final : public QObject {
    Q_OBJECT
public:
    StubDaemon() {
        connect(&m_server, &QLocalServer::newConnection, this, &StubDaemon::onConnection);
    }
    bool listen(const QString &path) { return m_server.listen(path); }
    bool audioOutput = true;
    int sets = 0;
    bool failNext = false;

private:
    QLocalServer m_server;

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
                result.insert(QStringLiteral("audio_output"), audioOutput);
            } else if (method == QStringLiteral("settings.set")) {
                ++sets;
                audioOutput = request.value(QStringLiteral("params"))
                                  .toObject()
                                  .value(QStringLiteral("audio_output"))
                                  .toBool();
                result.insert(QStringLiteral("audio_output"), audioOutput);
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

class SettingsClientTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_root.isValid());
        m_socketPath = m_root.path() + QStringLiteral("/daemon.sock");
        QVERIFY(m_daemon.listen(m_socketPath));
    }

    void init() {
        m_daemon.audioOutput = true;
        m_daemon.sets = 0;
        m_daemon.failNext = false;
    }

    void loadsTheDaemonValueAndSetsRoundTrip() {
        m_daemon.audioOutput = false;
        SettingsClient client(m_socketPath);
        QTRY_VERIFY(client.loaded());
        QVERIFY(!client.audioOutput());

        client.setAudioOutput(true);
        // Optimistic immediately, authoritative after the round trip.
        QVERIFY(client.audioOutput());
        QTRY_COMPARE(m_daemon.sets, 1);
        QTRY_VERIFY(!client.busy());
        QVERIFY(m_daemon.audioOutput);
        QVERIFY(client.errorMessage().isEmpty());
    }

    void daemonErrorSurfacesWithoutLosingState() {
        SettingsClient client(m_socketPath);
        QTRY_VERIFY(client.loaded());
        m_daemon.failNext = true;
        client.setAudioOutput(false);
        QTRY_VERIFY(!client.errorMessage().isEmpty());
        // A later successful refresh clears the error and restores the
        // daemon's authoritative value.
        client.refresh();
        QTRY_VERIFY(client.errorMessage().isEmpty());
        QTRY_VERIFY(client.audioOutput());
    }

private:
    QTemporaryDir m_root;
    StubDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(SettingsClientTest)
#include "settingsclienttest.moc"
