// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/occlusionbridge.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QTemporaryDir>
#include <QtTest>

// Daemon stand-in recording every `occlusion.report` it receives.
class StubDaemon final : public QObject {
    Q_OBJECT
public:
    StubDaemon() {
        connect(&m_server, &QLocalServer::newConnection, this, &StubDaemon::onConnection);
    }
    bool listen(const QString &path) { return m_server.listen(path); }
    QList<QJsonObject> reports;
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
            QJsonObject result;
            bool ok = request.value(QStringLiteral("method")).toString() ==
                      QStringLiteral("occlusion.report");
            if (ok)
                reports.push_back(request.value(QStringLiteral("params")).toObject());
            if (failNext) {
                failNext = false;
                ok = false;
                result.insert(QStringLiteral("error"), QStringLiteral("supervisor_failed"));
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

class OcclusionBridgeTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_root.isValid());
        m_socketPath = m_root.path() + QStringLiteral("/daemon.sock");
        QVERIFY(m_daemon.listen(m_socketPath));
    }

    void init() {
        m_daemon.reports.clear();
        m_daemon.failNext = false;
    }

    void parsesReportsAndRejectsMalformedOnes() {
        OcclusionBridge bridge(m_socketPath, 20);
        QVERIFY(bridge.applyReport(QStringLiteral(R"({"outputs":["HDMI-A-1","DP-1"],"covered":["DP-1"]})")));
        QCOMPARE(bridge.outputs(), (QStringList{QStringLiteral("DP-1"), QStringLiteral("HDMI-A-1")}));
        QCOMPARE(bridge.covered(), QStringList{QStringLiteral("DP-1")});
        for (const auto &bad : {QStringLiteral("not json"), QStringLiteral("[]"),
                                QStringLiteral(R"({"outputs":"DP-1","covered":[]})"),
                                QStringLiteral(R"({"outputs":[1],"covered":[]})"),
                                QStringLiteral(R"({"outputs":["DP-1"]})"),
                                QStringLiteral(R"({"outputs":["DP-1"],"covered":[""]})")}) {
            QVERIFY2(!bridge.applyReport(bad), qPrintable(bad));
            QCOMPARE(bridge.covered(), QStringList{QStringLiteral("DP-1")});
        }
    }

    void relaysDebouncedTransitionsLatestWins() {
        OcclusionBridge bridge(m_socketPath, 50);
        // Two transitions inside the debounce window collapse into one
        // relay carrying the latest view.
        bridge.Report(QStringLiteral(R"({"outputs":["DP-1"],"covered":[]})"));
        bridge.Report(QStringLiteral(R"({"outputs":["DP-1"],"covered":["DP-1"]})"));
        QTRY_COMPARE(bridge.relayedCount(), 1);
        QCOMPARE(m_daemon.reports.size(), 1);
        QCOMPARE(m_daemon.reports[0].value(QStringLiteral("covered")).toArray(),
                 QJsonArray{QStringLiteral("DP-1")});
        QCOMPARE(m_daemon.reports[0].value(QStringLiteral("outputs")).toArray(),
                 QJsonArray{QStringLiteral("DP-1")});
        // An identical report is not relayed again.
        bridge.Report(QStringLiteral(R"({"outputs":["DP-1"],"covered":["DP-1"]})"));
        QTest::qWait(150);
        QCOMPARE(bridge.relayedCount(), 1);
        // A malformed report is counted and ignored.
        bridge.Report(QStringLiteral("garbage"));
        QCOMPARE(bridge.rejectedCount(), 1);
        // reset() (KWin gone) relays an empty view → uncovered.
        bridge.reset();
        QTRY_COMPARE(bridge.relayedCount(), 2);
        QVERIFY(m_daemon.reports[1].value(QStringLiteral("outputs")).toArray().isEmpty());
    }

    void daemonErrorIsSurfacedAndLaterChangesStillRelay() {
        OcclusionBridge bridge(m_socketPath, 20);
        m_daemon.failNext = true;
        bridge.Report(QStringLiteral(R"({"outputs":["DP-1"],"covered":["DP-1"]})"));
        QTRY_VERIFY(!bridge.lastError().isEmpty());
        QCOMPARE(bridge.relayedCount(), 0);
        bridge.Report(QStringLiteral(R"({"outputs":["DP-1"],"covered":[]})"));
        QTRY_COMPARE(bridge.relayedCount(), 1);
        QVERIFY(bridge.lastError().isEmpty());
    }

private:
    QTemporaryDir m_root;
    StubDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(OcclusionBridgeTest)
#include "occlusionbridgetest.moc"
